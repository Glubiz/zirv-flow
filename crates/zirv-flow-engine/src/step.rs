//! The interpreter (architecture 9.2): one call advances through consecutive non-blocking nodes
//! and stops at a wait, a branch (which yields for its conditions), an exit or a failure.
//!
//! `step` assumes a graph that passed `core::flow::validate`, but never panics on one that did
//! not: it returns a typed [`EngineError`], and a walk longer than the node count is a cycle.

use chrono::{DateTime, TimeDelta, Utc};
use chrono_tz::Tz;
use serde_json::Value;
use uuid::Uuid;
use zirv_flow_core::flow::{
    EventMatch, FlowGraph, FlowNode, NodeKind, PORT_ELSE, PORT_EVENT, PORT_NEXT, PORT_TIMEOUT,
    arm_port, case_port,
};

use crate::error::EngineError;
use crate::hash::{effect_key, split_bucket, step_key, wake_key};
use crate::state::{
    Effect, EffectKind, EnrollmentState, ExitReason, Failure, Outcome, Status, StepContext,
    StepInput, Transition, Visit, WaitChange, Wake,
};
use crate::time::until;

/// Runs `input` against the enrollment at `state` and returns what to commit. The caller commits
/// it only if `state.state_version` is still current, so the first commit wins.
pub fn step(
    graph: &FlowGraph,
    state: &EnrollmentState,
    input: StepInput,
    ctx: &dyn StepContext,
) -> Result<Transition, EngineError> {
    let version = state
        .state_version
        .checked_add(1)
        .ok_or_else(|| EngineError::InvalidState("the state version is exhausted".into()))?;
    let mut run = Run {
        graph,
        state,
        ctx,
        version,
        effects: Vec::new(),
        visited: Vec::new(),
        waits: Vec::new(),
        zone_fallback: None,
    };
    match input {
        StepInput::Start => {
            run.require(Status::Active, "start")?;
            run.advance(state.node.clone())
        }
        StepInput::TimerDue { wake_version } => {
            if wake_version != state.state_version {
                return Ok(run.stale());
            }
            run.require(Status::Waiting, "a timer")?;
            let port = match &run.node(&state.node)?.kind {
                NodeKind::WaitDuration { .. } | NodeKind::WaitUntilTime { .. } => PORT_NEXT,
                NodeKind::WaitUntilEvent { .. } => PORT_TIMEOUT,
                other => return Err(not_a_wait(other)),
            };
            if port == PORT_TIMEOUT {
                run.clear_wait();
            }
            run.leave(&state.node, port)
        }
        StepInput::EventArrived { event } => {
            if event.wake_version != state.state_version {
                return Ok(run.stale());
            }
            run.require(Status::Waiting, "an event")?;
            let node = run.node(&state.node)?;
            if !matches!(node.kind, NodeKind::WaitUntilEvent { .. }) {
                return Err(not_a_wait(&node.kind));
            }
            if state.awaiting.as_deref() != Some(event.name.as_str()) {
                return Err(EngineError::InputMismatch(format!(
                    "the enrollment does not await `{}`",
                    event.name
                )));
            }
            run.clear_wait();
            run.leave(&state.node, PORT_EVENT)
        }
        StepInput::ConditionsEvaluated { node, results } => {
            run.require(Status::Active, "condition results")?;
            if node != state.node {
                return Err(EngineError::InputMismatch(format!(
                    "the enrollment is at `{}`, not `{node}`",
                    state.node
                )));
            }
            let NodeKind::Branch { conditions } = &run.node(&node)?.kind else {
                return Err(EngineError::InputMismatch(format!(
                    "`{node}` is not a branch"
                )));
            };
            if results.len() != conditions.len() {
                return Err(EngineError::InputMismatch(format!(
                    "{} results for {} conditions",
                    results.len(),
                    conditions.len()
                )));
            }
            let port = results
                .iter()
                .position(|matched| *matched)
                .map_or_else(|| PORT_ELSE.to_owned(), case_port);
            run.leave(&node, &port)
        }
        StepInput::Resume { skip } => {
            run.require(Status::Failed, "a resume")?;
            if skip {
                if !run.node(&state.node)?.kind.has_port(PORT_NEXT) {
                    return Err(EngineError::InputMismatch(format!(
                        "`{}` has no `next` port to skip along",
                        state.node
                    )));
                }
                run.leave(&state.node, PORT_NEXT)
            } else {
                run.advance(state.node.clone())
            }
        }
    }
}

fn not_a_wait(kind: &NodeKind) -> EngineError {
    EngineError::InvalidState(format!("a {} node is not asleep", kind.type_name()))
}

/// Why a walk stopped.
enum Halt {
    Wait {
        at: DateTime<Utc>,
        awaiting: Option<EventMatch>,
    },
    Yield(Vec<Value>),
    End(Outcome),
    Fail(Failure),
}

struct Run<'a> {
    graph: &'a FlowGraph,
    state: &'a EnrollmentState,
    ctx: &'a dyn StepContext,
    /// The version the new state gets.
    version: u64,
    effects: Vec<Effect>,
    visited: Vec<Visit>,
    waits: Vec<WaitChange>,
    zone_fallback: Option<String>,
}

impl Run<'_> {
    fn require(&self, status: Status, what: &str) -> Result<(), EngineError> {
        if self.state.status == status {
            Ok(())
        } else {
            Err(EngineError::InvalidState(format!(
                "{what} needs a {status:?} enrollment, not {:?}",
                self.state.status
            )))
        }
    }

    fn node(&self, id: &str) -> Result<&FlowNode, EngineError> {
        self.graph
            .node(id)
            .ok_or_else(|| EngineError::UnknownNode(id.to_owned()))
    }

    fn id(&self) -> Uuid {
        self.state.enrollment_id
    }

    /// No-op: the state as it was, nothing to commit.
    fn stale(self) -> Transition {
        Transition {
            state: self.state.clone(),
            visited: Vec::new(),
            effects: Vec::new(),
            wake: None,
            waits: Vec::new(),
            outcome: Outcome::Stale,
            zone_fallback: None,
        }
    }

    fn clear_wait(&mut self) {
        if let Some(name) = &self.state.awaiting {
            self.waits.push(WaitChange::Clear {
                event_name: name.clone(),
            });
        }
    }

    /// Leaves `node` through `port` and keeps going.
    fn leave(self, node: &str, port: &str) -> Result<Transition, EngineError> {
        let target = self.target(node, port)?;
        self.advance(target)
    }

    fn target(&self, node: &str, port: &str) -> Result<String, EngineError> {
        self.graph
            .edges
            .iter()
            .find(|edge| edge.from == node && edge.port == port)
            .map(|edge| edge.to.clone())
            .ok_or_else(|| EngineError::MissingEdge {
                node: node.to_owned(),
                port: port.to_owned(),
            })
    }

    /// Runs nodes from `first` until one blocks, ends or fails.
    fn advance(mut self, first: String) -> Result<Transition, EngineError> {
        let mut current = first;
        for _ in 0..=self.graph.nodes.len() {
            let graph = self.graph;
            let node = graph
                .node(&current)
                .ok_or_else(|| EngineError::UnknownNode(current.clone()))?;
            self.visited.push(Visit {
                node: node.id.clone(),
                step_key: step_key(self.id(), &node.id),
            });
            match self.execute(node) {
                Ok(port) => current = self.target(&node.id, &port)?,
                Err(halt) => return Ok(self.finish(&node.id, halt)),
            }
        }
        Err(EngineError::Cycle)
    }

    fn effect(&mut self, node: &FlowNode, external: Option<&str>, kind: EffectKind) {
        self.effects.push(Effect {
            node: node.id.clone(),
            step_key: step_key(self.id(), &node.id),
            effect_key: external.map(|name| effect_key(self.id(), &node.id, name)),
            kind,
        });
    }

    /// Runs one node: the port to leave through, or why the walk stops here.
    fn execute(&mut self, node: &FlowNode) -> Result<String, Halt> {
        let now = self.ctx.now();
        let next = || Ok(PORT_NEXT.to_owned());
        match &node.kind {
            NodeKind::WaitDuration { seconds } => match after(now, *seconds) {
                Some(at) => Err(Halt::Wait { at, awaiting: None }),
                None => Err(Halt::Fail(Failure::OutOfRange)),
            },
            NodeKind::WaitUntilTime { when } => {
                let zone = self.zone();
                match until(when, now, zone) {
                    Ok(at) => Err(Halt::Wait { at, awaiting: None }),
                    Err(_) => Err(Halt::Fail(Failure::InvalidTime)),
                }
            }
            NodeKind::WaitUntilEvent {
                event,
                timeout_seconds,
            } => match after(now, *timeout_seconds) {
                Some(at) => Err(Halt::Wait {
                    at,
                    awaiting: Some(event.clone()),
                }),
                None => Err(Halt::Fail(Failure::OutOfRange)),
            },
            NodeKind::Branch { conditions } => Err(Halt::Yield(conditions.clone())),
            NodeKind::SplitRandom { arms } => {
                let total: u64 = arms.iter().map(|arm| u64::from(arm.weight)).sum();
                if total == 0 {
                    return Err(Halt::Fail(Failure::EmptySplit));
                }
                let mut bucket = split_bucket(self.id(), &node.id) % total;
                let mut chosen = arms.len().saturating_sub(1);
                for (index, arm) in arms.iter().enumerate() {
                    let weight = u64::from(arm.weight);
                    if bucket < weight {
                        chosen = index;
                        break;
                    }
                    bucket -= weight;
                }
                Ok(arm_port(chosen))
            }
            NodeKind::ActionWebhook { url, trait_keys } => {
                self.effect(
                    node,
                    Some("webhook"),
                    EffectKind::Webhook {
                        url: url.clone(),
                        trait_keys: trait_keys.clone(),
                    },
                );
                next()
            }
            NodeKind::ActionUpdateTrait { key, change } => {
                self.effect(
                    node,
                    None,
                    EffectKind::UpdateTrait {
                        key: key.clone(),
                        change: change.clone(),
                    },
                );
                next()
            }
            NodeKind::ActionList { list_id, change } => {
                self.effect(
                    node,
                    None,
                    EffectKind::List {
                        list_id: *list_id,
                        change: *change,
                    },
                );
                next()
            }
            NodeKind::Goal { exit } => {
                self.effect(node, None, EffectKind::GoalConversion);
                if *exit {
                    Err(Halt::End(Outcome::Exited {
                        reason: ExitReason::Goal,
                    }))
                } else {
                    next()
                }
            }
            NodeKind::Exit {} => Err(Halt::End(Outcome::Completed)),
        }
    }

    /// The zone of a `wait.until_time`: the person's, else the workspace's. An invalid person
    /// zone is remembered for the step decision.
    fn zone(&mut self) -> Tz {
        match self.ctx.person_timezone() {
            Some(name) => name.parse::<Tz>().unwrap_or_else(|_| {
                self.zone_fallback = Some(name);
                self.ctx.workspace_timezone()
            }),
            None => self.ctx.workspace_timezone(),
        }
    }

    fn finish(mut self, node: &str, halt: Halt) -> Transition {
        let mut state = EnrollmentState {
            enrollment_id: self.state.enrollment_id,
            node: node.to_owned(),
            state_version: self.version,
            status: Status::Active,
            wake_at: None,
            awaiting: None,
        };
        let mut wake = None;
        let outcome = match halt {
            Halt::Wait { at, awaiting } => {
                state.status = Status::Waiting;
                state.wake_at = Some(at);
                wake = Some(Wake {
                    at,
                    key: wake_key(self.id(), self.version),
                });
                if let Some(event) = awaiting {
                    state.awaiting = Some(event.name.clone());
                    self.waits.push(WaitChange::Register {
                        event,
                        timeout_at: at,
                    });
                }
                Outcome::Waiting
            }
            Halt::Yield(conditions) => Outcome::NeedsConditions {
                node: node.to_owned(),
                conditions,
            },
            Halt::End(outcome) => {
                state.status = if outcome == Outcome::Completed {
                    Status::Completed
                } else {
                    Status::Exited
                };
                outcome
            }
            Halt::Fail(failure) => {
                state.status = Status::Failed;
                Outcome::Failed {
                    node: node.to_owned(),
                    failure,
                }
            }
        };
        Transition {
            state,
            visited: self.visited,
            effects: self.effects,
            wake,
            waits: self.waits,
            outcome,
            zone_fallback: self.zone_fallback,
        }
    }
}

/// `now` plus `seconds`, or `None` when that does not fit the clock.
fn after(now: DateTime<Utc>, seconds: u64) -> Option<DateTime<Utc>> {
    now.checked_add_signed(TimeDelta::try_seconds(i64::try_from(seconds).ok()?)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use proptest::prelude::*;
    use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};
    use serde_json::json;
    use zirv_flow_core::flow::{
        Arm, Edge, GRAPH_VERSION, ListChange, TraitChange, UntilTime, Weekday,
    };

    use crate::state::ArrivedEvent;

    struct Ctx {
        now: DateTime<Utc>,
        workspace: Tz,
        person: Option<String>,
    }

    impl StepContext for Ctx {
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

    fn utc(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, min, 0).unwrap()
    }

    fn ctx_at(now: DateTime<Utc>) -> Ctx {
        Ctx {
            now,
            workspace: Tz::UTC,
            person: None,
        }
    }

    fn ctx() -> Ctx {
        ctx_at(utc(2030, 1, 15, 8, 0))
    }

    fn enrollment() -> Uuid {
        Uuid::from_u128(0xabc)
    }

    /// A graph from `(id, kind)` nodes and `(from, port, to)` edges.
    fn graph(nodes: Vec<(&str, NodeKind)>, edges: &[(&str, &str, &str)]) -> FlowGraph {
        FlowGraph {
            v: GRAPH_VERSION,
            nodes: nodes
                .into_iter()
                .map(|(id, kind)| FlowNode {
                    id: id.into(),
                    kind,
                })
                .collect(),
            edges: edges
                .iter()
                .enumerate()
                .map(|(index, (from, port, to))| zirv_flow_core::flow::Edge {
                    id: format!("e{index}"),
                    from: (*from).into(),
                    port: (*port).into(),
                    to: (*to).into(),
                })
                .collect(),
        }
    }

    fn start(graph: &FlowGraph) -> EnrollmentState {
        EnrollmentState::new(enrollment(), graph).unwrap()
    }

    fn run(graph: &FlowGraph, state: &EnrollmentState, input: StepInput) -> Transition {
        step(graph, state, input, &ctx()).unwrap()
    }

    fn visited(transition: &Transition) -> Vec<&str> {
        transition.visited.iter().map(|v| v.node.as_str()).collect()
    }

    fn order() -> EventMatch {
        EventMatch {
            name: "order".into(),
            filters: vec![json!({"prop": "total", "op": "gt", "value": 100})],
        }
    }

    fn condition() -> Value {
        json!({"trait": "plan", "op": "eq", "value": "pro"})
    }

    fn until_event(timeout_seconds: u64) -> NodeKind {
        NodeKind::WaitUntilEvent {
            event: order(),
            timeout_seconds,
        }
    }

    fn arrived(name: &str, wake_version: u64) -> StepInput {
        StepInput::EventArrived {
            event: ArrivedEvent {
                name: name.into(),
                wake_version,
            },
        }
    }

    fn evaluated(node: &str, results: Vec<bool>) -> StepInput {
        StepInput::ConditionsEvaluated {
            node: node.into(),
            results,
        }
    }

    fn event_graph() -> FlowGraph {
        graph(
            vec![
                ("wait", until_event(600)),
                ("got", NodeKind::Exit {}),
                ("late", NodeKind::Exit {}),
            ],
            &[("wait", "event", "got"), ("wait", "timeout", "late")],
        )
    }

    // ---- every node kind ----

    #[test]
    fn actions_and_a_goal_run_in_order_and_the_flow_completes() {
        let list = Uuid::from_u128(9);
        let set = TraitChange::Set {
            value: json!("pro"),
        };
        let g = graph(
            vec![
                (
                    "trait",
                    NodeKind::ActionUpdateTrait {
                        key: "plan".into(),
                        change: set.clone(),
                    },
                ),
                (
                    "list",
                    NodeKind::ActionList {
                        list_id: list,
                        change: ListChange::Add,
                    },
                ),
                (
                    "hook",
                    NodeKind::ActionWebhook {
                        url: "https://example.com/hook".into(),
                        trait_keys: vec!["plan".into()],
                    },
                ),
                ("goal", NodeKind::Goal { exit: false }),
                ("end", NodeKind::Exit {}),
            ],
            &[
                ("trait", "next", "list"),
                ("list", "next", "hook"),
                ("hook", "next", "goal"),
                ("goal", "next", "end"),
            ],
        );
        let transition = run(&g, &start(&g), StepInput::Start);
        assert_eq!(
            visited(&transition),
            ["trait", "list", "hook", "goal", "end"]
        );
        assert_eq!(transition.outcome, Outcome::Completed);
        assert_eq!(transition.state.status, Status::Completed);
        assert_eq!(transition.state.state_version, 1);
        assert_eq!(transition.state.node, "end");
        assert!(transition.wake.is_none() && transition.waits.is_empty());

        let kinds: Vec<&EffectKind> = transition.effects.iter().map(|e| &e.kind).collect();
        assert_eq!(
            kinds,
            [
                &EffectKind::UpdateTrait {
                    key: "plan".into(),
                    change: set,
                },
                &EffectKind::List {
                    list_id: list,
                    change: ListChange::Add,
                },
                &EffectKind::Webhook {
                    url: "https://example.com/hook".into(),
                    trait_keys: vec!["plan".into()],
                },
                &EffectKind::GoalConversion,
            ]
        );
        let external: Vec<bool> = transition.effects.iter().map(Effect::is_external).collect();
        assert_eq!(external, [false, false, true, false]);
    }

    #[test]
    fn a_goal_with_exit_converts_and_exits() {
        let g = graph(vec![("goal", NodeKind::Goal { exit: true })], &[]);
        let transition = run(&g, &start(&g), StepInput::Start);
        assert_eq!(
            transition.outcome,
            Outcome::Exited {
                reason: ExitReason::Goal
            }
        );
        assert_eq!(transition.state.status, Status::Exited);
        assert_eq!(transition.effects.len(), 1);
        assert_eq!(transition.effects[0].kind, EffectKind::GoalConversion);
    }

    #[test]
    fn a_duration_wait_sleeps_then_a_timer_moves_on() {
        let g = graph(
            vec![
                ("wait", NodeKind::WaitDuration { seconds: 3600 }),
                ("end", NodeKind::Exit {}),
            ],
            &[("wait", "next", "end")],
        );
        let slept = run(&g, &start(&g), StepInput::Start);
        let at = utc(2030, 1, 15, 9, 0);
        assert_eq!(slept.outcome, Outcome::Waiting);
        assert_eq!(slept.state.status, Status::Waiting);
        assert_eq!(slept.state.wake_at, Some(at));
        assert_eq!(slept.state.state_version, 1);
        assert_eq!(
            slept.wake,
            Some(Wake {
                at,
                key: format!("wake:{}:1", enrollment()),
            })
        );
        assert!(slept.waits.is_empty() && slept.state.awaiting.is_none());

        let woken = run(&g, &slept.state, StepInput::TimerDue { wake_version: 1 });
        assert_eq!(woken.outcome, Outcome::Completed);
        assert_eq!(woken.state.state_version, 2);
        assert_eq!(visited(&woken), ["end"]);
        assert!(woken.wake.is_none() && woken.state.wake_at.is_none());
    }

    #[test]
    fn an_event_wait_registers_and_the_event_takes_the_event_port() {
        let g = event_graph();
        let slept = run(&g, &start(&g), StepInput::Start);
        let timeout_at = utc(2030, 1, 15, 8, 10);
        assert_eq!(slept.state.awaiting.as_deref(), Some("order"));
        assert_eq!(slept.state.wake_at, Some(timeout_at));
        assert_eq!(
            slept.waits,
            [WaitChange::Register {
                event: order(),
                timeout_at,
            }]
        );
        assert_eq!(slept.wake.as_ref().map(|w| w.at), Some(timeout_at));

        let got = run(&g, &slept.state, arrived("order", 1));
        assert_eq!(visited(&got), ["got"]);
        assert_eq!(
            got.waits,
            [WaitChange::Clear {
                event_name: "order".into()
            }]
        );
        assert!(got.state.awaiting.is_none() && got.state.wake_at.is_none());
    }

    #[test]
    fn an_event_wait_times_out_through_the_timeout_port() {
        let g = event_graph();
        let slept = run(&g, &start(&g), StepInput::Start);
        let timed_out = run(&g, &slept.state, StepInput::TimerDue { wake_version: 1 });
        assert_eq!(visited(&timed_out), ["late"]);
        assert_eq!(timed_out.outcome, Outcome::Completed);
        assert_eq!(
            timed_out.waits,
            [WaitChange::Clear {
                event_name: "order".into()
            }]
        );
    }

    // ---- branch ----

    #[test]
    fn a_branch_yields_its_conditions_with_the_effects_so_far() {
        let g = graph(
            vec![
                (
                    "before",
                    NodeKind::ActionUpdateTrait {
                        key: "seen".into(),
                        change: TraitChange::Unset {},
                    },
                ),
                (
                    "branch",
                    NodeKind::Branch {
                        conditions: vec![condition()],
                    },
                ),
                ("yes", NodeKind::Exit {}),
                ("no", NodeKind::Goal { exit: true }),
            ],
            &[
                ("before", "next", "branch"),
                ("branch", "case_0", "yes"),
                ("branch", "else", "no"),
            ],
        );
        let yielded = run(&g, &start(&g), StepInput::Start);
        assert_eq!(
            yielded.outcome,
            Outcome::NeedsConditions {
                node: "branch".into(),
                conditions: vec![condition()],
            }
        );
        assert_eq!(yielded.state.node, "branch");
        assert_eq!(yielded.state.status, Status::Active);
        assert_eq!(yielded.state.state_version, 1);
        assert_eq!(yielded.effects.len(), 1);
        assert!(yielded.wake.is_none());

        let matched = run(&g, &yielded.state, evaluated("branch", vec![true]));
        assert_eq!(visited(&matched), ["yes"]);
        assert_eq!(matched.outcome, Outcome::Completed);
        assert!(
            matched.effects.is_empty(),
            "the yielded effect is not repeated"
        );

        let unmatched = run(&g, &yielded.state, evaluated("branch", vec![false]));
        assert_eq!(visited(&unmatched), ["no"]);
    }

    #[test]
    fn the_first_matching_condition_wins() {
        let g = graph(
            vec![
                (
                    "branch",
                    NodeKind::Branch {
                        conditions: vec![condition(), condition(), condition()],
                    },
                ),
                ("a", NodeKind::Exit {}),
                ("b", NodeKind::Exit {}),
                ("c", NodeKind::Exit {}),
                ("other", NodeKind::Exit {}),
            ],
            &[
                ("branch", "case_0", "a"),
                ("branch", "case_1", "b"),
                ("branch", "case_2", "c"),
                ("branch", "else", "other"),
            ],
        );
        let yielded = run(&g, &start(&g), StepInput::Start);
        let pick = |results: Vec<bool>| {
            run(&g, &yielded.state, evaluated("branch", results))
                .state
                .node
        };
        assert_eq!(pick(vec![false, true, true]), "b");
        assert_eq!(pick(vec![false, false, true]), "c");
        assert_eq!(pick(vec![true, true, true]), "a");
        assert_eq!(pick(vec![false, false, false]), "other");
    }

    #[test]
    fn several_branches_yield_one_per_call_and_each_effect_is_yielded_once() {
        let set = |key: &str| NodeKind::ActionUpdateTrait {
            key: key.into(),
            change: TraitChange::Unset {},
        };
        let branch = NodeKind::Branch {
            conditions: vec![condition()],
        };
        let g = graph(
            vec![
                ("t0", set("a")),
                ("b1", branch.clone()),
                ("t1", set("b")),
                ("b2", branch),
                ("t2", set("c")),
                ("end", NodeKind::Exit {}),
            ],
            &[
                ("t0", "next", "b1"),
                ("b1", "case_0", "t1"),
                ("b1", "else", "t1"),
                ("t1", "next", "b2"),
                ("b2", "case_0", "t2"),
                ("b2", "else", "t2"),
                ("t2", "next", "end"),
            ],
        );
        let first = run(&g, &start(&g), StepInput::Start);
        assert_eq!(visited(&first), ["t0", "b1"]);
        let second = run(&g, &first.state, evaluated("b1", vec![true]));
        assert_eq!(visited(&second), ["t1", "b2"]);
        assert!(matches!(second.outcome, Outcome::NeedsConditions { .. }));
        let third = run(&g, &second.state, evaluated("b2", vec![false]));
        assert_eq!(visited(&third), ["t2", "end"]);
        assert_eq!(third.outcome, Outcome::Completed);
        assert_eq!(
            [
                first.state.state_version,
                second.state.state_version,
                third.state.state_version
            ],
            [1, 2, 3]
        );
        let nodes: Vec<&str> = [&first, &second, &third]
            .into_iter()
            .flat_map(|t| t.effects.iter().map(|e| e.node.as_str()))
            .collect();
        assert_eq!(nodes, ["t0", "t1", "t2"]);
    }

    // ---- split ----

    fn split_graph(weights: &[u32]) -> FlowGraph {
        let ends: Vec<String> = (0..weights.len()).map(|i| format!("end{i}")).collect();
        let ports: Vec<String> = (0..weights.len()).map(arm_port).collect();
        let mut nodes = vec![(
            "split",
            NodeKind::SplitRandom {
                arms: weights
                    .iter()
                    .map(|weight| Arm { weight: *weight })
                    .collect(),
            },
        )];
        let mut edges = Vec::new();
        for (index, end) in ends.iter().enumerate() {
            nodes.push((end.as_str(), NodeKind::Exit {}));
            edges.push(("split", ports[index].as_str(), end.as_str()));
        }
        graph(nodes, &edges)
    }

    fn arm_of(graph: &FlowGraph, id: u128) -> String {
        let state = EnrollmentState::new(Uuid::from_u128(id), graph).unwrap();
        run(graph, &state, StepInput::Start).state.node
    }

    #[test]
    fn a_split_follows_its_weights_over_ten_thousand_enrollments() {
        let g = split_graph(&[30, 70]);
        let first = (0..10_000_u128)
            .filter(|id| arm_of(&g, *id) == "end0")
            .count();
        // 30 percent of 10,000 is 3,000; the standard deviation is about 46.
        assert!((2_800..=3_200).contains(&first), "{first}");
    }

    #[test]
    fn a_split_is_stable_and_skips_zero_weight_arms() {
        let g = split_graph(&[30, 70]);
        let once: Vec<String> = (0..500_u128).map(|id| arm_of(&g, id)).collect();
        let again: Vec<String> = (0..500_u128).map(|id| arm_of(&g, id)).collect();
        assert_eq!(once, again);
        // Pinned: these enrollments land on the same arms in every release.
        let pinned: Vec<String> = (0..8_u128).map(|id| arm_of(&g, id)).collect();
        assert_eq!(pinned, PINNED_ARMS);

        let skewed = split_graph(&[0, 100, 0]);
        assert!((0..1_000_u128).all(|id| arm_of(&skewed, id) == "end1"));
        let single = split_graph(&[100]);
        assert!((0..100_u128).all(|id| arm_of(&single, id) == "end0"));
    }

    const PINNED_ARMS: [&str; 8] = [
        "end1", "end1", "end1", "end0", "end0", "end0", "end0", "end1",
    ];

    #[test]
    fn a_split_of_no_weight_fails_the_node() {
        let g = split_graph(&[0, 0]);
        let transition = run(&g, &start(&g), StepInput::Start);
        assert_eq!(
            transition.outcome,
            Outcome::Failed {
                node: "split".into(),
                failure: Failure::EmptySplit
            }
        );
        assert_eq!(transition.state.status, Status::Failed);
    }

    // ---- wait.until_time and zones ----

    fn until_graph(when: UntilTime) -> FlowGraph {
        graph(
            vec![
                ("wait", NodeKind::WaitUntilTime { when }),
                ("end", NodeKind::Exit {}),
            ],
            &[("wait", "next", "end")],
        )
    }

    fn at_nine() -> UntilTime {
        UntilTime::TimeOfDay {
            time: "09:00".into(),
            weekdays: vec![],
        }
    }

    #[test]
    fn until_time_uses_the_person_zone_then_the_workspace_zone() {
        let g = until_graph(at_nine());
        let mut context = ctx();
        context.workspace = Tz::America__New_York;
        let workspace = step(&g, &start(&g), StepInput::Start, &context).unwrap();
        // 08:00 UTC is 03:00 in New York in January: 09:00 local is 14:00 UTC the same day.
        assert_eq!(workspace.state.wake_at, Some(utc(2030, 1, 15, 14, 0)));
        assert!(workspace.zone_fallback.is_none());

        context.person = Some("Europe/Copenhagen".into());
        let person = step(&g, &start(&g), StepInput::Start, &context).unwrap();
        assert_eq!(person.state.wake_at, Some(utc(2030, 1, 16, 8, 0)));
        assert!(person.zone_fallback.is_none());
    }

    #[test]
    fn an_invalid_person_zone_falls_back_and_says_so() {
        let g = until_graph(at_nine());
        let mut context = ctx();
        context.workspace = Tz::America__New_York;
        context.person = Some("Mars/Olympus".into());
        let transition = step(&g, &start(&g), StepInput::Start, &context).unwrap();
        assert_eq!(transition.state.wake_at, Some(utc(2030, 1, 15, 14, 0)));
        assert_eq!(transition.zone_fallback.as_deref(), Some("Mars/Olympus"));
    }

    #[test]
    fn until_time_in_a_dst_gap_wakes_at_the_shifted_instant() {
        // Copenhagen, 2030-03-31: 02:00 jumps to 03:00. A wait until 02:30 wakes at 03:30 local.
        let g = until_graph(UntilTime::DateTime {
            at: "2030-03-31T02:30:00".into(),
        });
        let mut context = ctx_at(utc(2030, 3, 30, 12, 0));
        context.person = Some("Europe/Copenhagen".into());
        let transition = step(&g, &start(&g), StepInput::Start, &context).unwrap();
        assert_eq!(transition.state.wake_at, Some(utc(2030, 3, 31, 1, 30)));
    }

    #[test]
    fn until_time_in_a_dst_overlap_wakes_at_the_first_occurrence() {
        // Copenhagen, 2030-10-27: 03:00 falls back to 02:00, so 02:30 happens twice.
        let g = until_graph(UntilTime::DateTime {
            at: "2030-10-27T02:30:00".into(),
        });
        let mut context = ctx_at(utc(2030, 10, 26, 12, 0));
        context.person = Some("Europe/Copenhagen".into());
        let transition = step(&g, &start(&g), StepInput::Start, &context).unwrap();
        assert_eq!(transition.state.wake_at, Some(utc(2030, 10, 27, 0, 30)));
    }

    #[test]
    fn a_weekday_restricted_wait_lands_on_an_allowed_day() {
        let g = until_graph(UntilTime::TimeOfDay {
            time: "09:00".into(),
            weekdays: vec![Weekday::Fri],
        });
        // 2030-01-15 is a Tuesday.
        let transition = run(&g, &start(&g), StepInput::Start);
        assert_eq!(transition.state.wake_at, Some(utc(2030, 1, 18, 9, 0)));
    }

    #[test]
    fn an_unparseable_time_fails_the_node_and_an_operator_can_retry_or_skip() {
        let g = until_graph(UntilTime::DateTime { at: "soon".into() });
        let failed = run(&g, &start(&g), StepInput::Start);
        assert_eq!(
            failed.outcome,
            Outcome::Failed {
                node: "wait".into(),
                failure: Failure::InvalidTime
            }
        );
        assert_eq!(failed.state.status, Status::Failed);
        assert_eq!(failed.state.state_version, 1);

        let retried = run(&g, &failed.state, StepInput::Resume { skip: false });
        assert!(matches!(retried.outcome, Outcome::Failed { .. }));
        assert_eq!(retried.state.state_version, 2);

        let skipped = run(&g, &failed.state, StepInput::Resume { skip: true });
        assert_eq!(skipped.outcome, Outcome::Completed);
        assert_eq!(visited(&skipped), ["end"]);
    }

    #[test]
    fn a_retry_runs_only_the_failed_node_again() {
        let g = graph(
            vec![
                (
                    "hook",
                    NodeKind::ActionWebhook {
                        url: "https://example.com/hook".into(),
                        trait_keys: vec![],
                    },
                ),
                (
                    "wait",
                    NodeKind::WaitUntilTime {
                        when: UntilTime::DateTime { at: "soon".into() },
                    },
                ),
                ("end", NodeKind::Exit {}),
            ],
            &[("hook", "next", "wait"), ("wait", "next", "end")],
        );
        let failed = run(&g, &start(&g), StepInput::Start);
        let retried = run(&g, &failed.state, StepInput::Resume { skip: false });
        assert_eq!(failed.effects.len(), 1);
        assert!(retried.effects.is_empty());
        assert_eq!(visited(&retried), ["wait"]);
    }

    // ---- stale inputs ----

    #[test]
    fn a_stale_timer_or_event_is_a_no_op() {
        let g = event_graph();
        let slept = run(&g, &start(&g), StepInput::Start);
        for input in [
            StepInput::TimerDue { wake_version: 0 },
            StepInput::TimerDue { wake_version: 2 },
            arrived("order", 7),
        ] {
            let transition = run(&g, &slept.state, input);
            assert_eq!(transition.outcome, Outcome::Stale);
            assert_eq!(transition.state, slept.state);
            assert!(transition.effects.is_empty() && transition.visited.is_empty());
            assert!(transition.wake.is_none() && transition.waits.is_empty());
        }
    }

    #[test]
    fn the_first_commit_wins_when_an_event_races_its_timeout() {
        let g = event_graph();
        let slept = run(&g, &start(&g), StepInput::Start);
        let timer = StepInput::TimerDue { wake_version: 1 };
        let event = arrived("order", 1);
        // The timer commits first; the event, computed from the same state, meets the new one.
        let committed = run(&g, &slept.state, timer.clone());
        assert_eq!(visited(&committed), ["late"]);
        assert_eq!(
            run(&g, &committed.state, event.clone()).outcome,
            Outcome::Stale
        );
        // And the other way round.
        let committed = run(&g, &slept.state, event);
        assert_eq!(visited(&committed), ["got"]);
        assert_eq!(run(&g, &committed.state, timer).outcome, Outcome::Stale);
    }

    // ---- replays ----

    #[test]
    fn a_replay_of_the_same_input_yields_the_same_transition_and_keys() {
        let g = graph(
            vec![
                (
                    "hook",
                    NodeKind::ActionWebhook {
                        url: "https://example.com/hook".into(),
                        trait_keys: vec![],
                    },
                ),
                ("wait", NodeKind::WaitDuration { seconds: 60 }),
                ("end", NodeKind::Exit {}),
            ],
            &[("hook", "next", "wait"), ("wait", "next", "end")],
        );
        let state = start(&g);
        let first = run(&g, &state, StepInput::Start);
        let replay = run(&g, &state, StepInput::Start);
        assert_eq!(first, replay);

        let effect = &first.effects[0];
        let id = enrollment();
        assert_eq!(effect.step_key, format!("{id}:hook"));
        assert_eq!(
            effect.effect_key.as_deref(),
            Some(format!("{id}:hook:webhook").as_str())
        );
        assert_eq!(first.visited[0].step_key, effect.step_key);

        // Another enrollment of the same graph has other keys.
        let other = EnrollmentState::new(Uuid::from_u128(1), &g).unwrap();
        let other_run = run(&g, &other, StepInput::Start);
        assert_ne!(other_run.effects[0].step_key, effect.step_key);
        assert_ne!(other_run.effects[0].effect_key, effect.effect_key);
    }

    // ---- errors ----

    fn two_nodes() -> FlowGraph {
        graph(
            vec![
                ("wait", NodeKind::WaitDuration { seconds: 5 }),
                ("end", NodeKind::Exit {}),
            ],
            &[("wait", "next", "end")],
        )
    }

    #[test]
    fn inconsistent_input_is_a_typed_error() {
        let g = two_nodes();
        let state = start(&g);
        let slept = run(&g, &state, StepInput::Start).state;
        let attempt =
            |state: &EnrollmentState, input: StepInput| step(&g, state, input, &ctx()).unwrap_err();

        let mut lost = state.clone();
        lost.node = "nowhere".into();
        assert_eq!(
            attempt(&lost, StepInput::Start),
            EngineError::UnknownNode("nowhere".into())
        );
        // Start on a sleeping enrollment, a timer on an active one, resume on a healthy one.
        assert!(matches!(
            attempt(&slept, StepInput::Start),
            EngineError::InvalidState(_)
        ));
        assert!(matches!(
            attempt(&state, StepInput::TimerDue { wake_version: 0 }),
            EngineError::InvalidState(_)
        ));
        assert!(matches!(
            attempt(&slept, StepInput::Resume { skip: false }),
            EngineError::InvalidState(_)
        ));
        // Condition results for a node the enrollment is not at, or for a non-branch.
        assert!(matches!(
            attempt(&state, evaluated("end", vec![])),
            EngineError::InputMismatch(_)
        ));
        assert!(matches!(
            attempt(&state, evaluated("wait", vec![])),
            EngineError::InputMismatch(_)
        ));
        // An event for a wait that is a timer.
        assert!(matches!(
            attempt(&slept, arrived("order", 1)),
            EngineError::InvalidState(_)
        ));
        // The version cannot grow past the end.
        let mut worn = state;
        worn.state_version = u64::MAX;
        assert!(matches!(
            attempt(&worn, StepInput::Start),
            EngineError::InvalidState(_)
        ));
    }

    #[test]
    fn a_wrong_event_or_result_count_is_a_mismatch() {
        let g = graph(
            vec![
                ("wait", until_event(60)),
                (
                    "branch",
                    NodeKind::Branch {
                        conditions: vec![condition()],
                    },
                ),
                ("end", NodeKind::Exit {}),
            ],
            &[
                ("wait", "event", "branch"),
                ("wait", "timeout", "end"),
                ("branch", "case_0", "end"),
                ("branch", "else", "end"),
            ],
        );
        let slept = run(&g, &start(&g), StepInput::Start).state;
        assert!(matches!(
            step(&g, &slept, arrived("refund", 1), &ctx()),
            Err(EngineError::InputMismatch(_))
        ));
        let yielded = run(&g, &slept, arrived("order", 1)).state;
        assert!(matches!(
            step(&g, &yielded, evaluated("branch", vec![]), &ctx()),
            Err(EngineError::InputMismatch(_))
        ));
    }

    #[test]
    fn a_missing_edge_or_target_is_an_error_and_a_cycle_does_not_hang() {
        let unwired = graph(vec![("hook", NodeKind::Goal { exit: false })], &[]);
        assert_eq!(
            step(&unwired, &start(&unwired), StepInput::Start, &ctx()).unwrap_err(),
            EngineError::MissingEdge {
                node: "hook".into(),
                port: "next".into()
            }
        );
        let dangling = graph(
            vec![("hook", NodeKind::Goal { exit: false })],
            &[("hook", "next", "ghost")],
        );
        assert_eq!(
            step(&dangling, &start(&unwired), StepInput::Start, &ctx()).unwrap_err(),
            EngineError::UnknownNode("ghost".into())
        );
        let cycle = graph(
            vec![
                ("a", NodeKind::Goal { exit: false }),
                ("b", NodeKind::Goal { exit: false }),
            ],
            &[("a", "next", "b"), ("b", "next", "a")],
        );
        let state = EnrollmentState {
            node: "a".into(),
            ..start(&two_nodes())
        };
        assert_eq!(
            step(&cycle, &state, StepInput::Start, &ctx()).unwrap_err(),
            EngineError::Cycle
        );
    }

    #[test]
    fn skipping_a_node_without_a_next_port_is_refused() {
        let g = event_graph();
        let mut failed = EnrollmentState {
            node: "wait".into(),
            ..start(&g)
        };
        failed.status = Status::Failed;
        assert!(matches!(
            step(&g, &failed, StepInput::Resume { skip: true }, &ctx()),
            Err(EngineError::InputMismatch(_))
        ));
    }

    #[test]
    fn a_duration_beyond_the_clock_fails_the_node() {
        let g = graph(
            vec![
                ("wait", NodeKind::WaitDuration { seconds: u64::MAX }),
                ("end", NodeKind::Exit {}),
            ],
            &[("wait", "next", "end")],
        );
        let transition = run(&g, &start(&g), StepInput::Start);
        assert_eq!(
            transition.outcome,
            Outcome::Failed {
                node: "wait".into(),
                failure: Failure::OutOfRange
            }
        );
    }

    // ---- property: a forward-wired graph always gets to its exit ----

    /// A random forward-wired graph of `spec.len() + 1` nodes: node `i` leads only to nodes after
    /// it, and the last node is an exit.
    fn forward_graph(spec: &[(u8, u8, Vec<u16>)]) -> FlowGraph {
        let count = spec.len() + 1;
        let id = |index: usize| format!("n{index}");
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        for (index, (selector, size, picks)) in spec.iter().enumerate() {
            let kind = match selector % 7 {
                0 => NodeKind::WaitDuration { seconds: 60 },
                1 => until_event(600),
                2 => NodeKind::Branch {
                    conditions: vec![condition(); usize::from(*size)],
                },
                3 => NodeKind::SplitRandom {
                    arms: vec![Arm { weight: 50 }, Arm { weight: 50 }],
                },
                4 => NodeKind::ActionWebhook {
                    url: "https://example.com".into(),
                    trait_keys: vec![],
                },
                5 => NodeKind::ActionUpdateTrait {
                    key: "k".into(),
                    change: TraitChange::Unset {},
                },
                _ => NodeKind::Goal { exit: false },
            };
            for (slot, port) in kind.ports().into_iter().enumerate() {
                let span = count - index - 1;
                let pick = usize::from(picks[slot % picks.len()]) % span;
                edges.push(Edge {
                    id: format!("e{}", edges.len()),
                    from: id(index),
                    port,
                    to: id(index + 1 + pick),
                });
            }
            nodes.push(FlowNode {
                id: id(index),
                kind,
            });
        }
        nodes.push(FlowNode {
            id: id(count - 1),
            kind: NodeKind::Exit {},
        });
        FlowGraph {
            v: GRAPH_VERSION,
            nodes,
            edges,
        }
    }

    /// Drives a graph to its end with scripted answers; returns the visited node ids.
    fn drive(g: &FlowGraph, choices: &[bool]) -> Result<Vec<String>, String> {
        let mut state = start(g);
        let mut input = StepInput::Start;
        let mut seen: Vec<String> = Vec::new();
        let mut pick = choices.iter().cycle();
        for _ in 0..=g.nodes.len() {
            let transition = step(g, &state, input, &ctx()).map_err(|e| e.to_string())?;
            if transition.state.state_version != state.state_version + 1 {
                return Err("the version did not grow by one".into());
            }
            for visit in &transition.visited {
                if seen.contains(&visit.node) {
                    return Err(format!("{} visited twice", visit.node));
                }
                seen.push(visit.node.clone());
            }
            state = transition.state;
            input = match transition.outcome {
                Outcome::Completed => return Ok(seen),
                Outcome::Waiting if state.awaiting.is_some() && pick.next() == Some(&true) => {
                    arrived("order", state.state_version)
                }
                Outcome::Waiting => StepInput::TimerDue {
                    wake_version: state.state_version,
                },
                Outcome::NeedsConditions { node, conditions } => {
                    let results = conditions
                        .iter()
                        .map(|_| pick.next() == Some(&true))
                        .collect();
                    evaluated(&node, results)
                }
                other => return Err(format!("unexpected {other:?}")),
            };
        }
        Err("did not complete within the node count".into())
    }

    #[test]
    fn a_forward_wired_graph_always_completes_within_its_node_count() {
        let spec = proptest::collection::vec(
            (
                any::<u8>(),
                1_u8..4,
                proptest::collection::vec(any::<u16>(), 4),
            ),
            0..12,
        );
        let choices = proptest::collection::vec(any::<bool>(), 64);
        let mut seed = [0_u8; 32];
        for (index, byte) in seed.iter_mut().enumerate() {
            *byte = 7_u8.wrapping_add(u8::try_from(index).unwrap());
        }
        let mut runner = TestRunner::new_with_rng(
            Config {
                cases: 128,
                failure_persistence: None,
                ..Config::default()
            },
            TestRng::from_seed(RngAlgorithm::ChaCha, &seed),
        );
        runner
            .run(&(spec, choices), |(spec, choices)| {
                let g = forward_graph(&spec);
                let visited = drive(&g, &choices).map_err(TestCaseError::fail)?;
                prop_assert!(visited.len() <= g.nodes.len());
                prop_assert_eq!(
                    visited.last().map(String::as_str),
                    Some(g.nodes.last().unwrap().id.as_str())
                );
                Ok(())
            })
            .unwrap();
    }
}
