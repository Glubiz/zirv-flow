//! The data [`step`](crate::step) reads and returns: the persisted enrollment state, the inputs,
//! the injected context and the [`Transition`] to commit (architecture 9.2 and 9.3).

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;
use zirv_flow_core::flow::{EventMatch, FlowGraph, ListChange, TraitChange};

use crate::error::EngineError;

/// Where an enrollment is in its life.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Ready to run its node: new.
    Active,
    /// Yielded at a branch, waiting for the caller's condition results.
    AwaitingConditions,
    /// Asleep at a wait node until `wake_at` or, for `wait.until_event`, its event.
    Waiting,
    /// Reached an exit node.
    Completed,
    /// Left through a goal exit (the caller also ends enrollments for other reasons).
    Exited,
    /// Stopped at `node` until an operator retries or skips it.
    Failed,
}

/// The persisted state of one enrollment. `state_version` grows by one with every committed
/// transition, which is how stale jobs and events are told apart.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EnrollmentState {
    pub enrollment_id: Uuid,
    /// The node to run next, or the node the enrollment stopped at.
    pub node: String,
    pub state_version: u64,
    pub status: Status,
    /// When the timer of a wait fires (the timeout, for `wait.until_event`).
    pub wake_at: Option<DateTime<Utc>>,
    /// The event name a `wait.until_event` is waiting for.
    pub awaiting: Option<String>,
}

impl EnrollmentState {
    /// A new enrollment at the graph's entry node (the first node no edge leads to), version 0.
    pub fn new(enrollment_id: Uuid, graph: &FlowGraph) -> Result<Self, EngineError> {
        let entry = graph.entry().ok_or(EngineError::NoEntry)?;
        Ok(Self {
            enrollment_id,
            node: entry.id.clone(),
            state_version: 0,
            status: Status::Active,
            wake_at: None,
            awaiting: None,
        })
    }
}

/// An event that matched an enrollment's wait. The caller has applied the property filters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArrivedEvent {
    pub name: String,
    /// The `state_version` the wait was registered at, carried by the wait row.
    pub wake_version: u64,
}

/// What happened that the enrollment must react to.
#[derive(Clone, Debug, PartialEq)]
pub enum StepInput {
    /// Run the enrollment's first node.
    Start,
    /// The timer registered at `wake_version` is due.
    TimerDue { wake_version: u64 },
    /// An event the enrollment waits for arrived.
    EventArrived { event: ArrivedEvent },
    /// The caller's answer to a [`Outcome::NeedsConditions`]: one result per condition, in order.
    ConditionsEvaluated { node: String, results: Vec<bool> },
    /// An operator retries (`skip: false`) or skips (`skip: true`) a failed node.
    Resume { skip: bool },
}

/// What the engine may not know on its own: the time and the zones. It evaluates no conditions.
pub trait StepContext {
    fn now(&self) -> DateTime<Utc>;
    /// The workspace zone (an IANA name, validated when an Admin sets it).
    fn workspace_timezone(&self) -> Tz;
    /// The person's `timezone` trait, as written. An invalid name falls back to the workspace
    /// zone and is reported in [`Transition::zone_fallback`].
    fn person_timezone(&self) -> Option<String>;
}

/// A node the walk reached, for the `step_executions` rows and the test-mode path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Visit {
    pub node: String,
    pub step_key: String,
}

/// One effect of a step. Internal effects are applied in the transition's transaction, external
/// ones are enqueued as jobs in it.
#[derive(Clone, Debug, PartialEq)]
pub struct Effect {
    pub node: String,
    /// The step idempotency key, `(enrollment_id, node_id)`.
    pub step_key: String,
    /// The stable key of an external effect, derived from `step_key`; `None` for internal ones.
    pub effect_key: Option<String>,
    pub kind: EffectKind,
}

impl Effect {
    pub const fn is_external(&self) -> bool {
        self.effect_key.is_some()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum EffectKind {
    /// Internal: the flow's own contribution to a trait.
    UpdateTrait { key: String, change: TraitChange },
    /// Internal: list membership.
    List { list_id: Uuid, change: ListChange },
    /// Internal: a conversion.
    GoalConversion,
    /// External: the fixed envelope plus `trait_keys`, to `url`.
    Webhook {
        url: String,
        trait_keys: Vec<String>,
    },
}

/// The timer to enqueue: a `flow.step` job at `at` with the unique key `key`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Wake {
    pub at: DateTime<Utc>,
    pub key: String,
}

/// A change to the `enrollment_waits` rows of the enrollment.
#[derive(Clone, Debug, PartialEq)]
pub enum WaitChange {
    Register {
        event: EventMatch,
        timeout_at: DateTime<Utc>,
    },
    Clear {
        event_name: String,
    },
}

/// Why an enrollment left through the graph.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExitReason {
    /// A `goal` node with `exit`.
    Goal,
}

/// Why a node failed. An operator can retry or skip it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Failure {
    /// A wait time that does not parse or never comes.
    InvalidTime,
    /// A duration that does not fit the clock.
    OutOfRange,
    /// A split whose weights sum to zero.
    EmptySplit,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// Asleep at a wait.
    Waiting,
    /// At a branch: evaluate `conditions` for the person, then step with `ConditionsEvaluated`.
    NeedsConditions {
        node: String,
        conditions: Vec<Value>,
    },
    Completed,
    Exited {
        reason: ExitReason,
    },
    Failed {
        node: String,
        failure: Failure,
    },
    /// The input was for an older state: nothing to commit, the first commit won.
    Stale,
}

/// What to commit after a step, in one transaction: the state, the effects in order, the timer
/// and the wait rows.
#[derive(Clone, Debug, PartialEq)]
pub struct Transition {
    pub state: EnrollmentState,
    pub visited: Vec<Visit>,
    pub effects: Vec<Effect>,
    pub wake: Option<Wake>,
    pub waits: Vec<WaitChange>,
    pub outcome: Outcome,
    /// The invalid person zone the step fell back from, to record on its decision.
    pub zone_fallback: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use zirv_flow_core::flow::{Edge, FlowNode, GRAPH_VERSION, NodeKind, PORT_NEXT};

    fn node(id: &str, kind: NodeKind) -> FlowNode {
        FlowNode {
            id: id.into(),
            kind,
        }
    }

    #[test]
    fn a_new_enrollment_starts_at_the_entry_node() {
        // The entry is listed last: it is the node no edge leads to, not the first listed.
        let graph = FlowGraph {
            v: GRAPH_VERSION,
            nodes: vec![
                node("end", NodeKind::Exit {}),
                node("wait", NodeKind::WaitDuration { seconds: 5 }),
            ],
            edges: vec![Edge {
                id: "e".into(),
                from: "wait".into(),
                port: PORT_NEXT.into(),
                to: "end".into(),
            }],
        };
        let state = EnrollmentState::new(Uuid::nil(), &graph).unwrap();
        assert_eq!(state.node, "wait");
        assert_eq!(state.state_version, 0);
        assert_eq!(state.status, Status::Active);
        assert!(state.wake_at.is_none() && state.awaiting.is_none());
    }

    #[test]
    fn a_graph_without_an_entry_is_refused() {
        let empty = FlowGraph {
            v: GRAPH_VERSION,
            nodes: vec![],
            edges: vec![],
        };
        assert_eq!(
            EnrollmentState::new(Uuid::nil(), &empty),
            Err(EngineError::NoEntry)
        );
    }

    #[test]
    fn the_state_round_trips_through_json() {
        let state = EnrollmentState {
            enrollment_id: Uuid::nil(),
            node: "n".into(),
            state_version: 3,
            status: Status::Waiting,
            wake_at: Some(DateTime::<Utc>::UNIX_EPOCH),
            awaiting: Some("order".into()),
        };
        let wire = serde_json::to_value(&state).unwrap();
        assert_eq!(wire["status"], "waiting");
        assert_eq!(
            serde_json::from_value::<EnrollmentState>(wire).unwrap(),
            state
        );
    }
}
