//! The four invariants of the simulation, read from the model's tables after a run (and, for the
//! fourth, also at the 90-day mark). The checks read the audit ledger and the outbox as they are
//! and recompute every key from the graph, so a runtime that deduplicates wrongly, or not at
//! all, cannot hide behind its own bookkeeping.
//!
//! 1. No internal effect is applied twice (and none that ran is missing).
//! 2. Every external effect is enqueued exactly once, under its stable key.
//! 3. No timer fires before its `wake_at` (checked where the timer commits, in `run`).
//! 4. Every enrollment has ended, or waits on a pending wake or a registered event wait.

use std::collections::{BTreeMap, BTreeSet};

use uuid::Uuid;
use zirv_flow_core::flow::{FlowGraph, FlowNode, NodeKind};

use super::db::{Db, JobKind};
use super::{Invariant, Violation};
use crate::{Status, effect_key, step_key};

/// Everything the checks read.
pub(super) struct World<'a> {
    pub(super) db: &'a Db,
    pub(super) graphs: &'a [FlowGraph],
    /// Which graph each enrollment runs.
    pub(super) graph_of: &'a BTreeMap<Uuid, usize>,
}

impl World<'_> {
    /// The enrollment and node a step key names, and the node.
    fn node_of<'k>(&self, key: &'k str) -> Option<(Uuid, &'k str, &FlowNode)> {
        let (id, node) = key.split_once(':')?;
        let id = Uuid::parse_str(id).ok()?;
        let graph = self.graphs.get(*self.graph_of.get(&id)?)?;
        Some((id, node, graph.node(node)?))
    }
}

fn violation(out: &mut Vec<Violation>, invariant: Invariant, detail: String) {
    out.push(Violation { invariant, detail });
}

/// Invariant 1.
pub(super) fn internal_effects_once(world: &World<'_>, out: &mut Vec<Violation>) {
    let db = world.db;
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for key in &db.applied {
        *counts.entry(key.as_str()).or_default() += 1;
    }
    for (key, count) in &counts {
        if *count > 1 {
            violation(
                out,
                Invariant::InternalEffectsOnce,
                format!("the internal effect of step {key} was applied {count} times"),
            );
        }
        if !db.step_executions.contains(*key) {
            violation(
                out,
                Invariant::InternalEffectsOnce,
                format!(
                    "the internal effect of step {key} was applied but its step did not commit"
                ),
            );
        }
    }
    for key in &db.step_executions {
        let Some((_, _, node)) = world.node_of(key) else {
            continue;
        };
        let internal = matches!(
            node.kind,
            NodeKind::ActionUpdateTrait { .. }
                | NodeKind::ActionList { .. }
                | NodeKind::Goal { .. }
        );
        if internal && !counts.contains_key(key.as_str()) {
            violation(
                out,
                Invariant::InternalEffectsOnce,
                format!("step {key} ran but its internal effect was never applied"),
            );
        }
    }
}

/// Invariant 2.
pub(super) fn external_effects_once(world: &World<'_>, out: &mut Vec<Violation>) {
    let db = world.db;
    let mut rows: BTreeMap<&str, usize> = BTreeMap::new();
    for row in &db.outbox {
        *rows.entry(row.key.as_str()).or_default() += 1;
        let stable = effect_key(row.enrollment, &row.node, row.kind);
        if row.key != stable {
            violation(
                out,
                Invariant::ExternalEffectsOnce,
                format!(
                    "outbox row {} is not under its stable key {stable}",
                    row.key
                ),
            );
        }
        if !db
            .step_executions
            .contains(&step_key(row.enrollment, &row.node))
        {
            violation(
                out,
                Invariant::ExternalEffectsOnce,
                format!(
                    "outbox row {} belongs to a step that did not commit",
                    row.key
                ),
            );
        }
    }
    for (key, count) in &rows {
        if *count > 1 {
            violation(
                out,
                Invariant::ExternalEffectsOnce,
                format!("the external effect {key} was enqueued {count} times"),
            );
        }
    }
    for key in &db.step_executions {
        let Some((id, node_id, node)) = world.node_of(key) else {
            continue;
        };
        let (kind, url, trait_keys) = match &node.kind {
            NodeKind::ActionWebhook {
                url, trait_keys, ..
            } => ("webhook", url.as_str(), trait_keys.as_slice()),
            NodeKind::ActionSend { .. } => ("send", "", [].as_slice()),
            NodeKind::ActionSync { trait_keys, .. } => ("sync", "", trait_keys.as_slice()),
            _ => continue,
        };
        let stable = effect_key(id, node_id, kind);
        match db.outbox.iter().find(|row| row.key == stable) {
            None => violation(
                out,
                Invariant::ExternalEffectsOnce,
                format!("step {key} ran but its external effect {stable} was never enqueued"),
            ),
            Some(row) if row.kind != kind || row.url != url || row.trait_keys != trait_keys => {
                violation(
                    out,
                    Invariant::ExternalEffectsOnce,
                    format!("the external effect {stable} does not carry its node's url and keys"),
                )
            }
            Some(_) => {}
        }
    }
}

/// Invariant 4. `finished` is the end of the run, when nothing may still be waiting.
pub(super) fn liveness(world: &World<'_>, finished: bool, out: &mut Vec<Violation>) {
    let db = world.db;
    let mut wakes: BTreeSet<(Uuid, u64)> = BTreeSet::new();
    let mut starts: BTreeSet<Uuid> = BTreeSet::new();
    for job in db.queue.values() {
        match &job.kind {
            JobKind::Wake { version } => {
                wakes.insert((job.enrollment, *version));
            }
            JobKind::Start => {
                starts.insert(job.enrollment);
            }
            JobKind::Event { .. } => {}
        }
    }
    let stuck = |out: &mut Vec<Violation>, id: &Uuid, why: &str| {
        violation(
            out,
            Invariant::Liveness,
            format!("enrollment {id} is stuck: {why}"),
        );
    };
    for (id, state) in &db.enrollments {
        match state.status {
            Status::Completed | Status::Exited => {}
            Status::Waiting if finished => stuck(out, id, "still waiting after every job ran"),
            Status::Waiting => {
                if !wakes.contains(&(*id, state.state_version)) {
                    stuck(out, id, "waiting with no wake job for its state version");
                }
                if let Some(name) = &state.awaiting {
                    let registered = db
                        .waits
                        .get(&(*id, name.clone()))
                        .is_some_and(|row| row.wake_version == state.state_version);
                    if !registered {
                        stuck(out, id, "waiting for an event with no registered wait");
                    }
                }
            }
            Status::Active if !finished && starts.contains(id) => {}
            Status::Active => stuck(out, id, "never started"),
            Status::AwaitingConditions => stuck(out, id, "left at a branch"),
            Status::Failed => stuck(out, id, "failed"),
        }
    }
    for ((id, name), row) in &db.waits {
        let live = db.enrollments.get(id).is_some_and(|state| {
            state.status == Status::Waiting
                && state.state_version == row.wake_version
                && state.awaiting.as_deref() == Some(name.as_str())
        });
        if !live {
            violation(
                out,
                Invariant::Liveness,
                format!("a wait on `{name}` of enrollment {id} outlived its state"),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EnrollmentState;
    use zirv_flow_core::flow::{Edge, GRAPH_VERSION, PORT_NEXT, TraitChange};

    fn graph() -> FlowGraph {
        FlowGraph {
            v: GRAPH_VERSION,
            nodes: vec![
                FlowNode {
                    id: "set".into(),
                    kind: NodeKind::ActionUpdateTrait {
                        key: "k".into(),
                        change: TraitChange::Unset {},
                    },
                },
                FlowNode {
                    id: "end".into(),
                    kind: NodeKind::Exit {},
                },
            ],
            edges: vec![Edge {
                id: "e".into(),
                from: "set".into(),
                port: PORT_NEXT.into(),
                to: "end".into(),
            }],
        }
    }

    fn run(db: &Db, check: impl Fn(&World<'_>, &mut Vec<Violation>)) -> Vec<Violation> {
        let graphs = [graph()];
        let graph_of: BTreeMap<Uuid, usize> = db.enrollments.keys().map(|id| (*id, 0)).collect();
        let world = World {
            db,
            graphs: &graphs,
            graph_of: &graph_of,
        };
        let mut out = Vec::new();
        check(&world, &mut out);
        out
    }

    fn db(status: Status) -> (Db, Uuid) {
        let id = Uuid::from_u128(9);
        let mut db = Db::default();
        db.enrollments.insert(
            id,
            EnrollmentState {
                enrollment_id: id,
                node: "end".into(),
                state_version: 1,
                status,
                wake_at: None,
                awaiting: None,
            },
        );
        db.step_executions.insert(step_key(id, "set"));
        (db, id)
    }

    #[test]
    fn a_missing_internal_effect_is_reported() {
        let (db, _) = db(Status::Completed);
        let found = run(&db, internal_effects_once);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].invariant, Invariant::InternalEffectsOnce);
    }

    #[test]
    fn a_double_internal_effect_is_reported() {
        let (mut db, id) = db(Status::Completed);
        db.applied.push(step_key(id, "set"));
        assert!(run(&db, internal_effects_once).is_empty());
        db.applied.push(step_key(id, "set"));
        assert_eq!(run(&db, internal_effects_once).len(), 1);
    }

    #[test]
    fn a_waiting_enrollment_without_a_wake_is_stuck() {
        let (db, _) = db(Status::Waiting);
        let found = run(&db, |world, out| liveness(world, false, out));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].invariant, Invariant::Liveness);
    }

    #[test]
    fn an_ended_enrollment_is_live() {
        let (db, _) = db(Status::Completed);
        assert!(run(&db, |world, out| liveness(world, true, out)).is_empty());
    }
}
