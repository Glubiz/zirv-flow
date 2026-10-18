//! The in-memory transactional model of the tables the engine's output lands in (architecture
//! 9.2 and 9.3): enrollments with a `state_version`, step executions keyed by
//! `(enrollment_id, node_id)`, a job queue with unique keys, event waits, an outbox of external
//! effects keyed by the effect key, and an audit ledger of applied internal effects.
//!
//! [`Db::commit`] is one transaction. It writes in the order a real one would, keeps an undo
//! journal, and can be killed after any number of effects: a kill rolls everything back, which is
//! how a crash before the commit looks to the next reader. The ledger is the audit log the
//! invariants read; it is written where an effect is applied, not where it is deduplicated.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, TimeDelta, Utc};
use uuid::Uuid;

use super::Defect;
use crate::{Effect, EffectKind, EnrollmentState, Visit, WaitChange, Wake};

/// How the queue orders jobs: when they run, then a seeded tie-break, then arrival.
pub(super) type QueueKey = (DateTime<Utc>, u64, u64);

#[derive(Clone, Debug)]
pub(super) enum JobKind {
    /// `flow.step` for a new enrollment.
    Start,
    /// `flow.step` for the timer registered at `version`.
    Wake { version: u64 },
    /// An event for the enrollment arrived.
    Event { name: String },
}

#[derive(Clone, Debug)]
pub(super) struct Job {
    pub(super) enrollment: Uuid,
    pub(super) kind: JobKind,
    pub(super) run_at: DateTime<Utc>,
    pub(super) tie: u64,
    /// Crashes this job has been through so far.
    pub(super) crashes: u32,
    /// The state the first claim read. Only a runtime that does not re-read keeps using it.
    pub(super) snapshot: Option<EnrollmentState>,
}

/// An `enrollment_waits` row.
#[derive(Clone, Debug)]
pub(super) struct WaitRow {
    pub(super) wake_version: u64,
}

/// An outbox row: one external effect job, unique by its effect key.
#[derive(Clone, Debug)]
pub(super) struct OutboxRow {
    pub(super) key: String,
    pub(super) enrollment: Uuid,
    pub(super) node: String,
    pub(super) url: String,
    pub(super) trait_keys: Vec<String>,
}

/// What one step chain asks to commit, built before the transaction opens.
#[derive(Clone, Debug)]
pub(super) struct Tx {
    pub(super) enrollment: Uuid,
    /// The `state_version` the chain was computed from; the commit applies only if it is current.
    pub(super) base_version: u64,
    pub(super) state: EnrollmentState,
    pub(super) visits: Vec<Visit>,
    pub(super) effects: Vec<Effect>,
    pub(super) wake: Option<Wake>,
    pub(super) waits: Vec<WaitChange>,
    pub(super) decisions: Vec<String>,
    /// The `wake_at` of the timer this chain handled, if it handled one.
    pub(super) timer_for: Option<DateTime<Utc>>,
}

/// How a commit ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Commit {
    Committed,
    /// The state moved on since the chain was computed: nothing was written.
    Stale,
    /// The process died inside the transaction: everything was rolled back.
    RolledBack,
}

#[derive(Debug)]
enum Undo {
    StepExecution(String),
    Applied,
    Outbox(String),
    State(Uuid, EnrollmentState),
    Job(QueueKey, Option<String>),
    Wait((Uuid, String), Option<WaitRow>),
    Decision,
}

#[derive(Debug, Default)]
pub(super) struct Db {
    pub(super) enrollments: BTreeMap<Uuid, EnrollmentState>,
    pub(super) step_executions: BTreeSet<String>,
    pub(super) queue: BTreeMap<QueueKey, Job>,
    pub(super) job_keys: BTreeSet<String>,
    pub(super) waits: BTreeMap<(Uuid, String), WaitRow>,
    pub(super) outbox: Vec<OutboxRow>,
    outbox_keys: BTreeSet<String>,
    /// One entry (the step key) for every internal effect applied, in order.
    pub(super) applied: Vec<String>,
    pub(super) decisions: Vec<(Uuid, String)>,
    next_arrival: u64,
}

impl Db {
    /// Puts a job on the queue. A key already present is a no-op (`ON CONFLICT DO NOTHING`).
    pub(super) fn enqueue(&mut self, job: Job, key: Option<&str>) -> Option<QueueKey> {
        if let Some(key) = key
            && !self.job_keys.insert(key.to_owned())
        {
            return None;
        }
        Some(self.insert(job))
    }

    fn insert(&mut self, job: Job) -> QueueKey {
        self.next_arrival += 1;
        let key = (job.run_at, job.tie, self.next_arrival);
        self.queue.insert(key, job);
        key
    }

    /// Puts a claimed job back (the claim timed out after a crash), `delay` from `now`.
    pub(super) fn requeue(&mut self, mut job: Job, now: DateTime<Utc>, delay: TimeDelta, tie: u64) {
        job.run_at = now + delay;
        job.tie = tie;
        self.insert(job);
    }

    /// Commits `tx` as one transaction, or reports why not. `crash_after` kills the process after
    /// that many effects have been written (`0` is before the first, `effects + 1` is after
    /// every write but the commit itself); the writes are then rolled back.
    pub(super) fn commit(
        &mut self,
        tx: &Tx,
        crash_after: Option<usize>,
        defect: Option<Defect>,
        tie: u64,
    ) -> Commit {
        // The idempotency checks: the version compare-and-set, the step execution rows, and the
        // unique keys. A runtime without them is the broken model the tests must catch.
        let checked = defect != Some(Defect::NoIdempotency);
        let current = self
            .enrollments
            .get(&tx.enrollment)
            .map(|s| s.state_version);
        if checked && current != Some(tx.base_version) {
            return Commit::Stale;
        }

        let mut journal: Vec<Undo> = Vec::new();
        let mut fresh: BTreeSet<&str> = BTreeSet::new();
        for visit in &tx.visits {
            if self.step_executions.insert(visit.step_key.clone()) {
                journal.push(Undo::StepExecution(visit.step_key.clone()));
                fresh.insert(&visit.step_key);
            } else if !checked {
                fresh.insert(&visit.step_key);
            }
        }
        if crash_after == Some(0) {
            return self.roll_back(journal, defect);
        }

        for (index, effect) in tx.effects.iter().enumerate() {
            if fresh.contains(effect.step_key.as_str()) {
                self.apply(effect, tx.enrollment, checked, defect, &mut journal);
            }
            if crash_after == Some(index + 1) {
                return self.roll_back(journal, defect);
            }
        }

        let previous = self.enrollments.insert(tx.enrollment, tx.state.clone());
        if let Some(previous) = previous {
            journal.push(Undo::State(tx.enrollment, previous));
        }
        if let Some(wake) = &tx.wake
            && defect != Some(Defect::LoseWake)
        {
            let skew = if defect == Some(Defect::EarlyTimer) {
                TimeDelta::hours(-1)
            } else {
                TimeDelta::zero()
            };
            let job = Job {
                enrollment: tx.enrollment,
                kind: JobKind::Wake {
                    version: tx.state.state_version,
                },
                run_at: wake.at + skew,
                tie,
                crashes: 0,
                snapshot: None,
            };
            let unique = checked.then_some(wake.key.as_str());
            if unique.is_none() {
                self.job_keys.insert(wake.key.clone());
            }
            if let Some(queued) = self.enqueue(job, unique) {
                journal.push(Undo::Job(queued, Some(wake.key.clone())));
            }
        }
        for change in &tx.waits {
            match change {
                WaitChange::Register { event, .. } => {
                    let key = (tx.enrollment, event.name.clone());
                    let row = WaitRow {
                        wake_version: tx.state.state_version,
                    };
                    let before = self.waits.insert(key.clone(), row);
                    journal.push(Undo::Wait(key, before));
                }
                WaitChange::Clear { event_name } => {
                    let key = (tx.enrollment, event_name.clone());
                    let before = self.waits.remove(&key);
                    journal.push(Undo::Wait(key, before));
                }
            }
        }
        for decision in &tx.decisions {
            self.decisions.push((tx.enrollment, decision.clone()));
            journal.push(Undo::Decision);
        }
        if crash_after == Some(tx.effects.len() + 1) {
            return self.roll_back(journal, defect);
        }
        Commit::Committed
    }

    fn apply(
        &mut self,
        effect: &Effect,
        enrollment: Uuid,
        checked: bool,
        defect: Option<Defect>,
        journal: &mut Vec<Undo>,
    ) {
        let (EffectKind::Webhook { url, trait_keys }, Some(key)) =
            (&effect.kind, &effect.effect_key)
        else {
            self.applied.push(effect.step_key.clone());
            journal.push(Undo::Applied);
            return;
        };
        if defect == Some(Defect::DropOutbox) {
            return;
        }
        // The unique index on the effect key: a second insert changes nothing.
        if !self.outbox_keys.insert(key.clone()) && checked {
            return;
        }
        self.outbox.push(OutboxRow {
            key: key.clone(),
            enrollment,
            node: effect.node.clone(),
            url: url.clone(),
            trait_keys: trait_keys.clone(),
        });
        journal.push(Undo::Outbox(key.clone()));
    }

    fn roll_back(&mut self, journal: Vec<Undo>, defect: Option<Defect>) -> Commit {
        for undo in journal.into_iter().rev() {
            match undo {
                Undo::StepExecution(key) => {
                    self.step_executions.remove(&key);
                }
                // A runtime that applies internal effects outside the transaction cannot take
                // them back.
                Undo::Applied => {
                    if defect != Some(Defect::LeakyRollback) {
                        self.applied.pop();
                    }
                }
                Undo::Outbox(key) => {
                    self.outbox.pop();
                    self.outbox_keys.remove(&key);
                }
                Undo::State(id, previous) => {
                    self.enrollments.insert(id, previous);
                }
                Undo::Job(key, unique) => {
                    self.queue.remove(&key);
                    if let Some(unique) = unique {
                        self.job_keys.remove(&unique);
                    }
                }
                Undo::Wait(key, before) => match before {
                    Some(row) => {
                        self.waits.insert(key, row);
                    }
                    None => {
                        self.waits.remove(&key);
                    }
                },
                Undo::Decision => {
                    self.decisions.pop();
                }
            }
        }
        Commit::RolledBack
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Status, effect_key, step_key, wake_key};
    use zirv_flow_core::flow::{ListChange, TraitChange};

    fn state(id: Uuid, version: u64) -> EnrollmentState {
        EnrollmentState {
            enrollment_id: id,
            node: "n0".into(),
            state_version: version,
            status: Status::Waiting,
            wake_at: Some(DateTime::<Utc>::UNIX_EPOCH),
            awaiting: None,
        }
    }

    fn tx(id: Uuid, base: u64) -> Tx {
        let internal = Effect {
            node: "a".into(),
            step_key: step_key(id, "a"),
            effect_key: None,
            kind: EffectKind::UpdateTrait {
                key: "k".into(),
                change: TraitChange::Unset {},
            },
        };
        let list = Effect {
            node: "b".into(),
            step_key: step_key(id, "b"),
            effect_key: None,
            kind: EffectKind::List {
                list_id: Uuid::nil(),
                change: ListChange::Add,
            },
        };
        let hook = Effect {
            node: "c".into(),
            step_key: step_key(id, "c"),
            effect_key: Some(effect_key(id, "c", "webhook")),
            kind: EffectKind::Webhook {
                url: "https://example.com".into(),
                trait_keys: Vec::new(),
            },
        };
        let visits = ["a", "b", "c"]
            .iter()
            .map(|node| Visit {
                node: (*node).into(),
                step_key: step_key(id, node),
            })
            .collect();
        Tx {
            enrollment: id,
            base_version: base,
            state: state(id, base + 1),
            visits,
            effects: vec![internal, list, hook],
            wake: Some(Wake {
                at: DateTime::<Utc>::UNIX_EPOCH,
                key: wake_key(id, base + 1),
            }),
            waits: Vec::new(),
            decisions: vec!["zone".into()],
            timer_for: None,
        }
    }

    fn db_with(id: Uuid) -> Db {
        let mut db = Db::default();
        db.enrollments.insert(id, state(id, 0));
        db
    }

    #[test]
    fn a_commit_writes_everything_once() {
        let id = Uuid::from_u128(1);
        let mut db = db_with(id);
        assert_eq!(db.commit(&tx(id, 0), None, None, 0), Commit::Committed);
        assert_eq!(db.applied.len(), 2);
        assert_eq!(db.outbox.len(), 1);
        assert_eq!(db.queue.len(), 1);
        assert_eq!(db.enrollments[&id].state_version, 1);
        assert_eq!(db.decisions.len(), 1);
        // The same chain again is stale and writes nothing.
        assert_eq!(db.commit(&tx(id, 0), None, None, 0), Commit::Stale);
        assert_eq!(
            (db.applied.len(), db.outbox.len(), db.queue.len()),
            (2, 1, 1)
        );
    }

    #[test]
    fn a_crash_at_any_point_rolls_everything_back() {
        let id = Uuid::from_u128(2);
        for point in 0..=4 {
            let mut db = db_with(id);
            let before = db.enrollments[&id].clone();
            assert_eq!(
                db.commit(&tx(id, 0), Some(point), None, 0),
                Commit::RolledBack,
                "point {point}"
            );
            assert_eq!(db.enrollments[&id], before, "point {point}");
            assert!(db.step_executions.is_empty(), "point {point}");
            assert!(
                db.applied.is_empty() && db.outbox.is_empty(),
                "point {point}"
            );
            assert!(
                db.queue.is_empty() && db.job_keys.is_empty(),
                "point {point}"
            );
            assert!(db.decisions.is_empty(), "point {point}");
            // And the retry commits normally.
            assert_eq!(db.commit(&tx(id, 0), None, None, 0), Commit::Committed);
            assert_eq!(db.applied.len(), 2);
        }
    }

    #[test]
    fn a_runtime_without_the_checks_applies_twice() {
        let id = Uuid::from_u128(3);
        let mut db = db_with(id);
        let defect = Some(Defect::NoIdempotency);
        assert_eq!(db.commit(&tx(id, 0), None, defect, 0), Commit::Committed);
        assert_eq!(db.commit(&tx(id, 0), None, defect, 0), Commit::Committed);
        assert_eq!(db.applied.len(), 4);
        assert_eq!(db.outbox.len(), 2);
    }
}
