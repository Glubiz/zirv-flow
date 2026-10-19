//! The test-mode walk (architecture 9.4): one person through a graph on a virtual clock. It is
//! pure over its inputs, so it writes nothing and a replay of the same inputs is identical.
//!
//! It runs `engine::step` and answers each `NeedsConditions` itself with the reference
//! evaluator over the snapshot, with the walk's own internal effects overlaid first (traits set,
//! incremented and unset; lists added and removed), exactly where the runtime would have applied
//! them before evaluating. Timers fire when the clock reaches them; an injected event satisfies
//! the wait it names, and is added to the snapshot's events.

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use serde_json::{Map, Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;
use zirv_flow_core::flow::{
    EventMatch, FlowGraph, ListChange, NodeKind, PORT_ELSE, PORT_EVENT, PORT_NEXT, PORT_TIMEOUT,
    TraitChange, case_port,
};
use zirv_flow_engine::{
    ArrivedEvent, Effect, EffectKind, EngineError, EnrollmentState, Outcome, Status, StepContext,
    StepInput, Transition, step,
};
use zirv_flow_segments::{
    Catalog, EvalNode, Observed, Outcome as Verdict, PersonSnapshot, SnapshotEvent, evaluate,
    event_lower_bounds, parse_and_validate,
};

use super::body::{
    ConditionBody, EffectBody, FinalStateBody, HaltBody, ObservedBody, PathStepBody,
    PersonAfterBody, ScriptResultBody, WaitBody, WakeBody,
};

/// The most script steps one walk runs.
pub(crate) const MAX_SCRIPT_STEPS: usize = 50;
/// The most `engine::step` calls one walk makes. A graph is acyclic and holds at most
/// `MAX_NODES` nodes, so a walk that reaches this is a graph far beyond what publish accepts.
pub(crate) const MAX_STEPS: usize = 1_000;

/// One step of the script.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Step {
    AdvanceTo(DateTime<Utc>),
    Inject {
        name: String,
        properties: Map<String, Value>,
        at: DateTime<Utc>,
    },
}

impl Step {
    const fn at(&self) -> DateTime<Utc> {
        match self {
            Self::AdvanceTo(at) | Self::Inject { at, .. } => *at,
        }
    }
}

/// Who and what is being walked, for keys and the webhook envelope.
pub(crate) struct Subject {
    pub(crate) flow_id: Uuid,
    pub(crate) version: Option<i32>,
    pub(crate) person_id: Option<Uuid>,
    pub(crate) enrollment_id: Uuid,
}

pub(crate) struct Walk<'a> {
    pub(crate) graph: &'a FlowGraph,
    pub(crate) catalog: &'a Catalog,
    pub(crate) subject: &'a Subject,
    /// The workspace zone, for `wait.until_time`.
    pub(crate) zone: Tz,
    pub(crate) snapshot: PersonSnapshot,
    pub(crate) start_at: DateTime<Utc>,
    pub(crate) script: &'a [Step],
    pub(crate) max_steps: usize,
    /// The event cap a real load applies, named by `unable_to_evaluate`.
    pub(crate) event_cap: usize,
}

/// Why a walk is refused rather than answered.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Refusal {
    /// More `engine::step` calls than the cap.
    StepLimit,
    /// The script step at `index` goes back in time.
    ScriptOrder { index: usize },
    /// The graph has no entry node.
    NoEntry,
}

/// Everything a finished walk reports.
pub(crate) struct Walked {
    pub(crate) path: Vec<PathStepBody>,
    pub(crate) internal_effects: Vec<EffectBody>,
    pub(crate) external_effects: Vec<EffectBody>,
    pub(crate) waits: Vec<WaitBody>,
    pub(crate) wakes: Vec<WakeBody>,
    pub(crate) script: Vec<ScriptResultBody>,
    pub(crate) final_state: FinalStateBody,
    pub(crate) halted: Option<HaltBody>,
    pub(crate) person_after: PersonAfterBody,
}

/// The wire time of an instant. A chrono instant outside the range of `time` clamps to the epoch.
pub(crate) fn to_time(at: DateTime<Utc>) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(at.timestamp_micros()) * 1_000)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

/// The engine's instant for a wire time, truncated to microseconds as the database holds it.
pub(crate) fn to_chrono(at: OffsetDateTime) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp_micros(i64::try_from(at.unix_timestamp_nanos() / 1_000).ok()?)
}

/// Runs the walk.
pub(crate) fn walk(input: Walk<'_>) -> Result<Walked, Refusal> {
    let mut previous = input.start_at;
    for (index, step) in input.script.iter().enumerate() {
        if step.at() < previous {
            return Err(Refusal::ScriptOrder { index });
        }
        previous = step.at();
    }
    let state = EnrollmentState::new(input.subject.enrollment_id, input.graph)
        .map_err(|_| Refusal::NoEntry)?;
    let mut walker = Walker {
        input: &input,
        state,
        clock: input.start_at,
        snapshot: input.snapshot.clone(),
        steps: 0,
        path: Vec::new(),
        internal: Vec::new(),
        external: Vec::new(),
        waits: Vec::new(),
        wakes: Vec::new(),
        results: Vec::new(),
        halted: None,
    };
    walker.advance(StepInput::Start)?;
    for (index, scripted) in input.script.iter().enumerate() {
        let (kind, at) = match scripted {
            Step::AdvanceTo(at) => ("advance_to", *at),
            Step::Inject { at, .. } => ("inject", *at),
        };
        let outcome = if walker.halted.is_some() {
            "skipped"
        } else {
            match scripted {
                Step::AdvanceTo(target) => {
                    walker.fire_timers(*target, true)?;
                    "advanced"
                }
                Step::Inject {
                    name,
                    properties,
                    at,
                } => walker.inject(name, properties, *at)?,
            }
        };
        walker.results.push(ScriptResultBody {
            index,
            kind,
            at: to_time(at),
            outcome,
        });
    }
    Ok(walker.finish())
}

struct Ctx {
    now: DateTime<Utc>,
    zone: Tz,
    person_zone: Option<String>,
}

impl StepContext for Ctx {
    fn now(&self) -> DateTime<Utc> {
        self.now
    }

    fn workspace_timezone(&self) -> Tz {
        self.zone
    }

    fn person_timezone(&self) -> Option<String> {
        self.person_zone.clone()
    }
}

struct Walker<'a> {
    input: &'a Walk<'a>,
    state: EnrollmentState,
    clock: DateTime<Utc>,
    /// The person as the evaluator sees them now: the input, the overlay and injected events.
    snapshot: PersonSnapshot,
    steps: usize,
    path: Vec<PathStepBody>,
    internal: Vec<EffectBody>,
    external: Vec<EffectBody>,
    waits: Vec<WaitBody>,
    wakes: Vec<WakeBody>,
    results: Vec<ScriptResultBody>,
    halted: Option<HaltBody>,
}

fn halt(reason: &'static str, node: Option<&str>, message: impl Into<String>) -> HaltBody {
    HaltBody {
        reason,
        node: node.map(str::to_owned),
        condition: None,
        pointer: None,
        capped_events: Vec::new(),
        cap: None,
        message: message.into(),
    }
}

/// The port of the only edge from `from` to `to`; none when there are several, so a split whose
/// arms meet is not guessed at.
fn unique_port(graph: &FlowGraph, from: &str, to: &str) -> Option<String> {
    let mut edges = graph
        .edges
        .iter()
        .filter(|edge| edge.from == from && edge.to == to);
    let first = edges.next()?;
    edges.next().is_none().then(|| first.port.clone())
}

fn verdict_name(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Pass => "pass",
        Verdict::Fail => "fail",
        Verdict::Unknown => "unknown",
    }
}

fn observed_value(seen: &Observed) -> Value {
    match seen {
        Observed::Missing => json!({"missing": true}),
        Observed::Stored(value) => json!({"value": value}),
        Observed::Matching { events } => json!({"count": events}),
        Observed::Aggregate { value, events } => json!({"value": value, "count": events}),
        Observed::Present(held) => json!({"present": held}),
    }
}

/// The leaves of an evaluation tree with what each saw.
fn leaves(node: &EvalNode, into: &mut Vec<ObservedBody>) {
    if node.children.is_empty() {
        into.push(ObservedBody {
            pointer: node.pointer.clone(),
            outcome: verdict_name(node.outcome),
            observed: match (&node.observed, node.outcome) {
                (Some(seen), Verdict::Pass | Verdict::Fail) => observed_value(seen),
                _ => Value::Null,
            },
        });
    }
    for child in &node.children {
        leaves(child, into);
    }
}

/// The pointer of the first leaf that could not be decided.
fn first_unknown(node: &EvalNode) -> Option<&str> {
    if node.outcome != Verdict::Unknown {
        return None;
    }
    node.children
        .iter()
        .find_map(first_unknown)
        .or_else(|| node.children.is_empty().then_some(node.pointer.as_str()))
}

fn number_value(sum: f64) -> Option<Value> {
    if !sum.is_finite() {
        return None;
    }
    if sum.fract() == 0.0 && sum.abs() < 9.0e15 {
        Some(Value::from(sum as i64))
    } else {
        serde_json::Number::from_f64(sum).map(Value::Number)
    }
}

impl Walker<'_> {
    fn graph(&self) -> &FlowGraph {
        self.input.graph
    }

    fn node_type(&self, node: &str) -> &'static str {
        self.graph()
            .node(node)
            .map_or("unknown", |found| found.kind.type_name())
    }

    fn person_zone(&self) -> Option<String> {
        self.snapshot
            .traits
            .get("timezone")
            .and_then(Value::as_str)
            .map(str::to_owned)
    }

    /// Runs `input`, and every follow-up the walk answers itself, until the enrollment stops.
    fn advance(&mut self, first: StepInput) -> Result<(), Refusal> {
        let mut input = first;
        loop {
            if self.steps >= self.input.max_steps {
                return Err(Refusal::StepLimit);
            }
            self.steps += 1;
            let ctx = Ctx {
                now: self.clock,
                zone: self.input.zone,
                person_zone: self.person_zone(),
            };
            let transition = match step(self.input.graph, &self.state, input, &ctx) {
                Ok(transition) => transition,
                Err(error) => {
                    self.halted = Some(engine_halt(&error));
                    return Ok(());
                }
            };
            let outcome = transition.outcome.clone();
            self.absorb(transition);
            if self.halted.is_some() {
                return Ok(());
            }
            let Outcome::NeedsConditions { node, conditions } = outcome else {
                return Ok(());
            };
            let Some(results) = self.decide(&node, &conditions) else {
                return Ok(());
            };
            input = StepInput::ConditionsEvaluated { node, results };
        }
    }

    /// Records what a step did and moves the state on.
    fn absorb(&mut self, transition: Transition) {
        let at = to_time(self.clock);
        for (index, visit) in transition.visited.iter().enumerate() {
            let port = transition
                .visited
                .get(index + 1)
                .and_then(|next| unique_port(self.graph(), &visit.node, &next.node));
            self.path.push(PathStepBody {
                node: visit.node.clone(),
                node_type: self.node_type(&visit.node),
                at,
                outcome: "passed",
                port,
                branch: None,
                woke_at: None,
                woke_by: None,
            });
        }
        let last = match &transition.outcome {
            Outcome::Waiting => Some("waiting"),
            Outcome::NeedsConditions { .. } => Some("branched"),
            Outcome::Completed => Some("completed"),
            Outcome::Exited { .. } => Some("exited"),
            Outcome::Failed { .. } => Some("failed"),
            Outcome::Stale => None,
        };
        if let (Some(outcome), false, Some(entry)) =
            (last, transition.visited.is_empty(), self.path.last_mut())
        {
            entry.outcome = outcome;
        }
        for effect in &transition.effects {
            self.apply(effect);
            if self.halted.is_some() {
                break;
            }
        }
        if matches!(transition.outcome, Outcome::Waiting) {
            let node = transition.state.node.clone();
            if let Some(wake_at) = transition.state.wake_at {
                self.waits.push(WaitBody {
                    node_type: self.node_type(&node),
                    node,
                    started_at: at,
                    wake_at: to_time(wake_at),
                    awaiting: transition.state.awaiting.clone(),
                    resolved: "open",
                });
            }
        }
        if let Some(wake) = &transition.wake {
            self.wakes.push(WakeBody {
                node: transition.state.node.clone(),
                at: to_time(wake.at),
                key: wake.key.clone(),
                fired: false,
            });
        }
        self.state = transition.state;
    }

    /// Captures an effect, and overlays an internal one on the snapshot the conditions read.
    fn apply(&mut self, effect: &Effect) {
        let at = to_time(self.clock);
        let body = |kind: &'static str, detail: Value| EffectBody {
            node: effect.node.clone(),
            at,
            step_key: effect.step_key.clone(),
            effect_key: effect.effect_key.clone(),
            kind,
            detail,
        };
        match &effect.kind {
            EffectKind::UpdateTrait { key, change } => {
                let change_json = serde_json::to_value(change).unwrap_or(Value::Null);
                match self.change_trait(key, change) {
                    Ok(()) => {
                        let result = self.snapshot.traits.get(key).cloned();
                        self.internal.push(body(
                            "update_trait",
                            json!({"key": key, "change": change_json, "result": result}),
                        ));
                    }
                    Err(message) => {
                        self.internal.push(body(
                            "update_trait",
                            json!({"key": key, "change": change_json, "error": message}),
                        ));
                        self.halted = Some(halt("step_failed", Some(&effect.node), message));
                    }
                }
            }
            EffectKind::List { list_id, change } => {
                match change {
                    ListChange::Add => self.snapshot.lists.insert(*list_id),
                    ListChange::Remove => self.snapshot.lists.remove(list_id),
                };
                self.internal.push(body(
                    "list",
                    json!({"list_id": list_id, "change": serde_json::to_value(change).unwrap_or(Value::Null)}),
                ));
            }
            EffectKind::GoalConversion => self.internal.push(body("goal", json!({}))),
            EffectKind::Webhook { url, trait_keys } => {
                let traits: Map<String, Value> = trait_keys
                    .iter()
                    .map(|key| {
                        (
                            key.clone(),
                            self.snapshot
                                .traits
                                .get(key)
                                .cloned()
                                .unwrap_or(Value::Null),
                        )
                    })
                    .collect();
                let subject = self.input.subject;
                let envelope = json!({
                    "flow_id": subject.flow_id,
                    "version": subject.version,
                    "node_id": effect.node,
                    "enrollment_id": subject.enrollment_id,
                    "person_id": subject.person_id,
                    "trace_id": null,
                    "idempotency_key": effect.effect_key,
                    "occurred_at": at.format(&Rfc3339).unwrap_or_default(),
                    "traits": traits,
                });
                self.external
                    .push(body("webhook", json!({"url": url, "envelope": envelope})));
            }
        }
    }

    /// Applies a trait change to the overlay, as the runtime would write the flow's contribution.
    /// An unset removes the key: the snapshot holds the effective value only, so a value another
    /// source holds is not restored.
    fn change_trait(&mut self, key: &str, change: &TraitChange) -> Result<(), String> {
        match change {
            TraitChange::Set { value } => {
                self.snapshot.traits.insert(key.to_owned(), value.clone());
            }
            TraitChange::Unset {} => {
                self.snapshot.traits.remove(key);
            }
            TraitChange::Increment { by } => {
                let base = match self.snapshot.traits.get(key) {
                    None | Some(Value::Null) => 0.0,
                    Some(Value::Number(number)) => number.as_f64().unwrap_or(0.0),
                    Some(_) => return Err(format!("trait `{key}` is not a number")),
                };
                let sum = number_value(base + by)
                    .ok_or_else(|| format!("incrementing `{key}` does not give a number"))?;
                self.snapshot.traits.insert(key.to_owned(), sum);
            }
        }
        Ok(())
    }

    /// Evaluates a branch's conditions in order over the overlaid snapshot. The first that
    /// passes wins, so later ones are not evaluated. `None` when the walk has to stop.
    fn decide(&mut self, node: &str, conditions: &[Value]) -> Option<Vec<bool>> {
        let now = to_time(self.clock);
        let mut results = vec![false; conditions.len()];
        let mut bodies = Vec::with_capacity(conditions.len());
        let mut matched = None;
        for (index, condition) in conditions.iter().enumerate() {
            if matched.is_some() {
                bodies.push(ConditionBody {
                    index,
                    outcome: "not_evaluated",
                    observed: Vec::new(),
                });
                continue;
            }
            let mut wire = condition.as_object().cloned().unwrap_or_default();
            wire.insert("v".to_owned(), 1.into());
            let definition = match parse_and_validate(&Value::Object(wire), self.input.catalog) {
                Ok(definition) if condition.is_object() => definition,
                Ok(_) => return self.stop_invalid(node, index, bodies, "not a condition"),
                Err(issues) => {
                    let first = issues.first().map_or_else(String::new, |issue| {
                        format!("{} at {}", issue.code, issue.pointer)
                    });
                    return self.stop_invalid(node, index, bodies, &first);
                }
            };
            let evaluation = evaluate(&definition, self.input.catalog, &self.snapshot, now);
            let mut observed = Vec::new();
            leaves(&evaluation.tree, &mut observed);
            bodies.push(ConditionBody {
                index,
                outcome: verdict_name(evaluation.outcome),
                observed,
            });
            match evaluation.outcome {
                Verdict::Pass => {
                    results[index] = true;
                    matched = Some(index);
                }
                Verdict::Fail => {}
                Verdict::Unknown => {
                    let capped_events = event_lower_bounds(&definition, now)
                        .into_keys()
                        .filter(|name| self.snapshot.capped_events.contains(name))
                        .collect();
                    self.halted = Some(HaltBody {
                        reason: "unable_to_evaluate",
                        node: Some(node.to_owned()),
                        condition: Some(index),
                        pointer: first_unknown(&evaluation.tree).map(str::to_owned),
                        capped_events,
                        cap: Some(self.input.event_cap),
                        message: format!(
                            "condition {index} of `{node}` reads events the snapshot capped, so it \
                             cannot be decided"
                        ),
                    });
                    self.close_branch(bodies, None, "unable_to_evaluate");
                    return None;
                }
            }
        }
        let port = matched.map_or_else(|| PORT_ELSE.to_owned(), case_port);
        self.close_branch(bodies, Some(port), "branched");
        Some(results)
    }

    fn stop_invalid(
        &mut self,
        node: &str,
        index: usize,
        bodies: Vec<ConditionBody>,
        why: &str,
    ) -> Option<Vec<bool>> {
        let mut stopped = halt(
            "invalid_condition",
            Some(node),
            format!("condition {index} of `{node}` does not validate: {why}"),
        );
        stopped.condition = Some(index);
        self.halted = Some(stopped);
        self.close_branch(bodies, None, "failed");
        None
    }

    fn close_branch(
        &mut self,
        bodies: Vec<ConditionBody>,
        port: Option<String>,
        outcome: &'static str,
    ) {
        if let Some(entry) = self.path.last_mut() {
            entry.branch = Some(bodies);
            entry.port = port;
            entry.outcome = outcome;
        }
    }

    /// Fires every timer due before `until` (at or before it when `inclusive`), then moves the
    /// clock there.
    fn fire_timers(&mut self, until: DateTime<Utc>, inclusive: bool) -> Result<(), Refusal> {
        while self.halted.is_none() && self.state.status == Status::Waiting {
            let Some(wake) = self.state.wake_at else {
                break;
            };
            if wake > until || (!inclusive && wake == until) {
                break;
            }
            self.clock = self.clock.max(wake);
            self.mark_woke(if self.state.awaiting.is_some() {
                "timeout"
            } else {
                "timer"
            });
            let wake_version = self.state.state_version;
            self.advance(StepInput::TimerDue { wake_version })?;
        }
        self.clock = self.clock.max(until);
        Ok(())
    }

    /// Delivers an injected event at `at`: the timers due before it fire first, and the event
    /// lands in the snapshot whether or not it wakes anything.
    fn inject(
        &mut self,
        name: &str,
        properties: &Map<String, Value>,
        at: DateTime<Utc>,
    ) -> Result<&'static str, Refusal> {
        self.fire_timers(at, false)?;
        if self.halted.is_some() {
            return Ok("skipped");
        }
        let event = SnapshotEvent {
            name: name.to_owned(),
            occurred_at: to_time(at),
            properties: properties.clone(),
        };
        self.snapshot.events.push(event.clone());
        let awaited =
            self.state.status == Status::Waiting && self.state.awaiting.as_deref() == Some(name);
        let wanted = match self
            .input
            .graph
            .node(&self.state.node)
            .map(|found| &found.kind)
        {
            Some(NodeKind::WaitUntilEvent { event: wanted, .. }) if awaited => wanted.clone(),
            _ => return Ok("event_recorded"),
        };
        if !self.matches(&wanted, event) {
            return Ok(if self.halted.is_some() {
                "skipped"
            } else {
                "event_ignored"
            });
        }
        self.mark_woke("event");
        let wake_version = self.state.state_version;
        self.advance(StepInput::EventArrived {
            event: ArrivedEvent {
                name: name.to_owned(),
                wake_version,
            },
        })?;
        Ok("event_delivered")
    }

    /// Whether `event` passes the property filters of the wait, by the reference evaluator over
    /// a snapshot of that one event. A filter that does not validate stops the walk.
    fn matches(&mut self, wanted: &EventMatch, event: SnapshotEvent) -> bool {
        let mut wire = json!({"v": 1, "event": wanted.name});
        if !wanted.filters.is_empty() {
            wire["where"] = Value::Array(wanted.filters.clone());
        }
        let definition = match parse_and_validate(&wire, self.input.catalog) {
            Ok(definition) => definition,
            Err(issues) => {
                let first = issues.first().map_or_else(String::new, |issue| {
                    format!("{} at {}", issue.code, issue.pointer)
                });
                self.halted = Some(halt(
                    "invalid_condition",
                    Some(&self.state.node),
                    format!(
                        "the event filters of `{}` do not validate: {first}",
                        self.state.node
                    ),
                ));
                return false;
            }
        };
        let alone = PersonSnapshot {
            events: vec![event],
            ..PersonSnapshot::default()
        };
        evaluate(&definition, self.input.catalog, &alone, to_time(self.clock)).outcome
            == Verdict::Pass
    }

    /// Notes that the wait the enrollment sleeps at woke now, and by what.
    fn mark_woke(&mut self, by: &'static str) {
        let node = self.state.node.clone();
        let at = to_time(self.clock);
        if let Some(entry) = self
            .path
            .iter_mut()
            .rev()
            .find(|entry| entry.node == node && entry.woke_by.is_none())
        {
            entry.outcome = "passed";
            entry.woke_at = Some(at);
            entry.woke_by = Some(by);
            entry.port = Some(
                match by {
                    "event" => PORT_EVENT,
                    "timeout" => PORT_TIMEOUT,
                    _ => PORT_NEXT,
                }
                .to_owned(),
            );
        }
        if let Some(wait) = self
            .waits
            .iter_mut()
            .rev()
            .find(|wait| wait.node == node && wait.resolved == "open")
        {
            wait.resolved = by;
        }
        if let Some(wake) = self
            .wakes
            .iter_mut()
            .rev()
            .find(|wake| wake.node == node && !wake.fired)
        {
            wake.fired = by != "event";
        }
    }

    fn finish(self) -> Walked {
        let status = match self.state.status {
            Status::Active => "active",
            Status::AwaitingConditions => "awaiting_conditions",
            Status::Waiting => "waiting",
            Status::Completed => "completed",
            Status::Exited => "exited",
            Status::Failed => "failed",
        };
        Walked {
            path: self.path,
            internal_effects: self.internal,
            external_effects: self.external,
            waits: self.waits,
            wakes: self.wakes,
            script: self.results,
            final_state: FinalStateBody {
                status: status.to_owned(),
                node: self.state.node,
                state_version: self.state.state_version,
                wake_at: self.state.wake_at.map(to_time),
                awaiting: self.state.awaiting,
                clock: to_time(self.clock),
            },
            halted: self.halted,
            person_after: PersonAfterBody {
                traits: self.snapshot.traits,
                lists: self.snapshot.lists.into_iter().collect(),
            },
        }
    }
}

fn engine_halt(error: &EngineError) -> HaltBody {
    halt("engine_error", None, error.to_string())
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use hashbrown::HashMap;
    use zirv_flow_core::events::PropertyType;
    use zirv_flow_core::people::TraitType;

    use super::*;

    const LIST: Uuid = Uuid::from_u128(0x018f_0000_0000_7000_8000_0000_0000_0001);

    fn at(seconds: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_750_000_000 + seconds, 0).unwrap()
    }

    fn catalog() -> Catalog {
        let mut catalog = Catalog::default();
        catalog.traits.insert("plan".into(), TraitType::String);
        catalog.traits.insert("visits".into(), TraitType::Number);
        let mut order = HashMap::new();
        order.insert("total".to_owned(), PropertyType::Number);
        catalog.events.insert("order".to_owned(), order);
        catalog.lists.insert(LIST);
        catalog
    }

    fn graph(nodes: Value, edges: &[(&str, &str, &str)]) -> FlowGraph {
        let edges: Vec<Value> = edges
            .iter()
            .enumerate()
            .map(|(index, (from, port, to))| {
                json!({"id": format!("e{index}"), "from": from, "port": port, "to": to})
            })
            .collect();
        FlowGraph::from_value(json!({"v": 1, "nodes": nodes, "edges": edges})).unwrap()
    }

    fn subject() -> Subject {
        Subject {
            flow_id: Uuid::from_u128(1),
            version: Some(3),
            person_id: None,
            enrollment_id: Uuid::from_u128(2),
        }
    }

    fn run(
        graph: &FlowGraph,
        snapshot: PersonSnapshot,
        script: &[Step],
        max_steps: usize,
    ) -> Result<Walked, Refusal> {
        walk(Walk {
            graph,
            catalog: &catalog(),
            subject: &subject(),
            zone: Tz::UTC,
            snapshot,
            start_at: at(0),
            script,
            max_steps,
            event_cap: 10_000,
        })
    }

    fn inject(name: &str, properties: Value, seconds: i64) -> Step {
        Step::Inject {
            name: name.into(),
            properties: properties.as_object().cloned().unwrap_or_default(),
            at: at(seconds),
        }
    }

    fn nodes_of(walked: &Walked) -> Vec<(&str, &str)> {
        walked
            .path
            .iter()
            .map(|step| (step.node.as_str(), step.outcome))
            .collect()
    }

    fn capped_branch() -> FlowGraph {
        graph(
            json!([
                {"id": "b", "type": "branch", "conditions": [{"event": "order"}]},
                {"id": "end", "type": "exit"}
            ]),
            &[("b", "case_0", "end"), ("b", "else", "end")],
        )
    }

    /// Wait, set a trait, branch on it, split, wait for an event with a timeout, then either a
    /// goal and the end, or a webhook and a goal that exits.
    fn journey() -> FlowGraph {
        graph(
            json!([
                {"id": "w1", "type": "wait.duration", "seconds": 60},
                {"id": "set", "type": "action.update_trait", "key": "plan",
                 "change": {"op": "set", "value": "pro"}},
                {"id": "b", "type": "branch",
                 "conditions": [{"trait": "plan", "op": "eq", "value": "pro"}]},
                {"id": "sp", "type": "split.random", "arms": [{"weight": 50}, {"weight": 50}]},
                {"id": "ev", "type": "wait.until_event", "event": "order",
                 "filters": [{"prop": "total", "op": "gt", "value": 100}],
                 "timeout_seconds": 600},
                {"id": "won", "type": "goal"},
                {"id": "end", "type": "exit"},
                {"id": "hook", "type": "action.webhook", "url": "https://example.com/hook",
                 "trait_keys": ["plan"]},
                {"id": "lost", "type": "goal", "exit": true},
                {"id": "other", "type": "exit"}
            ]),
            &[
                ("w1", "next", "set"),
                ("set", "next", "b"),
                ("b", "case_0", "sp"),
                ("b", "else", "other"),
                ("sp", "arm_0", "ev"),
                ("sp", "arm_1", "ev"),
                ("ev", "event", "won"),
                ("ev", "timeout", "hook"),
                ("won", "next", "end"),
                ("hook", "next", "lost"),
            ],
        )
    }

    #[test]
    fn a_walk_goes_through_a_wait_a_branch_a_split_and_an_event_that_arrives() {
        let walked = run(
            &journey(),
            PersonSnapshot::default(),
            &[
                inject("order", json!({"total": 5}), 120),
                inject("order", json!({"total": 500}), 180),
            ],
            MAX_STEPS,
        )
        .unwrap();

        assert_eq!(
            nodes_of(&walked),
            [
                ("w1", "passed"),
                ("set", "passed"),
                ("b", "branched"),
                ("sp", "passed"),
                ("ev", "passed"),
                ("won", "passed"),
                ("end", "completed"),
            ]
        );
        // The wait woke by its timer at +60; the branch took case_0 on the value the walk set.
        assert_eq!(walked.path[0].woke_by, Some("timer"));
        assert_eq!(walked.path[0].woke_at, Some(to_time(at(60))));
        let branch = walked.path[2].branch.as_ref().unwrap();
        assert_eq!(branch[0].outcome, "pass");
        assert_eq!(branch[0].observed[0].observed, json!({"value": "pro"}));
        assert_eq!(walked.path[2].port.as_deref(), Some("case_0"));
        // Both arms lead to the same node, so the arm is not guessed at.
        assert_eq!(walked.path[3].port, None);
        // The first order failed the filter and stayed recorded; the second woke the wait.
        assert_eq!(walked.script[0].outcome, "event_ignored");
        assert_eq!(walked.script[1].outcome, "event_delivered");
        assert_eq!(walked.path[4].woke_by, Some("event"));
        assert_eq!(walked.path[4].woke_at, Some(to_time(at(180))));
        assert_eq!(walked.path[4].port.as_deref(), Some("event"));
        assert_eq!(walked.waits.len(), 2);
        assert_eq!(walked.waits[1].resolved, "event");
        assert_eq!(walked.waits[1].awaiting.as_deref(), Some("order"));
        assert_eq!(walked.final_state.status, "completed");
        assert!(walked.halted.is_none() && walked.external_effects.is_empty());
        let kinds: Vec<&str> = walked.internal_effects.iter().map(|e| e.kind).collect();
        assert_eq!(kinds, ["update_trait", "goal"]);
        assert_eq!(walked.person_after.traits["plan"], "pro");
    }

    #[test]
    fn an_event_wait_that_times_out_takes_the_timeout_edge_and_a_goal_can_exit() {
        let walked = run(
            &journey(),
            PersonSnapshot::default(),
            &[Step::AdvanceTo(at(10_000))],
            MAX_STEPS,
        )
        .unwrap();

        assert_eq!(
            nodes_of(&walked),
            [
                ("w1", "passed"),
                ("set", "passed"),
                ("b", "branched"),
                ("sp", "passed"),
                ("ev", "passed"),
                ("hook", "passed"),
                ("lost", "exited"),
            ]
        );
        assert_eq!(walked.path[4].woke_by, Some("timeout"));
        assert_eq!(walked.path[4].port.as_deref(), Some("timeout"));
        assert_eq!(walked.final_state.status, "exited");
        assert_eq!(walked.final_state.clock, to_time(at(10_000)));
        // The webhook is captured with its envelope and key; nothing is signed or sent.
        let [hook] = walked.external_effects.as_slice() else {
            panic!("one external effect");
        };
        let subject = subject();
        assert_eq!(hook.kind, "webhook");
        assert_eq!(
            hook.effect_key.as_deref(),
            Some(format!("{}:hook:webhook", subject.enrollment_id).as_str())
        );
        let envelope = &hook.detail["envelope"];
        assert_eq!(envelope["node_id"], "hook");
        assert_eq!(envelope["version"], 3);
        assert_eq!(envelope["traits"], json!({"plan": "pro"}));
        assert_eq!(envelope["idempotency_key"], json!(hook.effect_key));
        assert!(hook.detail.get("signature").is_none() && envelope.get("secret").is_none());
        assert_eq!(walked.wakes.iter().filter(|wake| wake.fired).count(), 2);
    }

    #[test]
    fn a_branch_reads_the_traits_and_lists_the_walk_changed() {
        let graph = graph(
            json!([
                {"id": "inc", "type": "action.update_trait", "key": "visits",
                 "change": {"op": "increment", "by": 2}},
                {"id": "join", "type": "action.list", "list_id": LIST, "change": "add"},
                {"id": "b", "type": "branch", "conditions": [
                    {"trait": "visits", "op": "gt", "value": 2},
                    {"list": LIST}
                ]},
                {"id": "big", "type": "exit"},
                {"id": "listed", "type": "exit"},
                {"id": "none", "type": "exit"}
            ]),
            &[
                ("inc", "next", "join"),
                ("join", "next", "b"),
                ("b", "case_0", "big"),
                ("b", "case_1", "listed"),
                ("b", "else", "none"),
            ],
        );
        let mut snapshot = PersonSnapshot::default();
        snapshot.traits.insert("visits".into(), json!(1));

        let walked = run(&graph, snapshot, &[], MAX_STEPS).unwrap();

        // 1 + 2 = 3 is over 2, and the second condition is not evaluated after the first passes.
        assert_eq!(walked.path.last().unwrap().node, "big");
        let branch = walked.path[2].branch.as_ref().unwrap();
        assert_eq!(branch[0].observed[0].observed, json!({"value": 3}));
        assert_eq!(branch[1].outcome, "not_evaluated");
        assert_eq!(walked.person_after.lists, [LIST]);

        // With the count still low, the list the walk joined decides.
        let mut low = PersonSnapshot::default();
        low.traits.insert("visits".into(), json!(-5));
        let walked = run(&graph, low, &[], MAX_STEPS).unwrap();
        assert_eq!(walked.path.last().unwrap().node, "listed");
        assert_eq!(walked.path[2].port.as_deref(), Some("case_1"));
    }

    #[test]
    fn incrementing_a_trait_that_is_not_a_number_fails_the_step() {
        let graph = graph(
            json!([
                {"id": "inc", "type": "action.update_trait", "key": "plan",
                 "change": {"op": "increment", "by": 1}},
                {"id": "end", "type": "exit"}
            ]),
            &[("inc", "next", "end")],
        );
        let mut snapshot = PersonSnapshot::default();
        snapshot.traits.insert("plan".into(), json!("pro"));

        let walked = run(&graph, snapshot, &[], MAX_STEPS).unwrap();

        let halted = walked.halted.unwrap();
        assert_eq!(halted.reason, "step_failed");
        assert_eq!(halted.node.as_deref(), Some("inc"));
        assert_eq!(walked.internal_effects[0].detail["error"], halted.message);
    }

    #[test]
    fn a_capped_condition_stops_the_walk_and_keeps_the_partial_path() {
        let graph = graph(
            json!([
                {"id": "w", "type": "wait.duration", "seconds": 5},
                {"id": "b", "type": "branch", "conditions": [{"event": "order"}]},
                {"id": "yes", "type": "exit"},
                {"id": "no", "type": "exit"}
            ]),
            &[
                ("w", "next", "b"),
                ("b", "case_0", "yes"),
                ("b", "else", "no"),
            ],
        );
        let mut snapshot = PersonSnapshot::default();
        snapshot.capped_events.insert("order".into());

        let walked = run(&graph, snapshot, &[Step::AdvanceTo(at(60))], MAX_STEPS).unwrap();

        assert_eq!(
            nodes_of(&walked),
            [("w", "passed"), ("b", "unable_to_evaluate")]
        );
        let halted = walked.halted.unwrap();
        assert_eq!(halted.reason, "unable_to_evaluate");
        assert_eq!(halted.node.as_deref(), Some("b"));
        assert_eq!(halted.condition, Some(0));
        assert_eq!(halted.capped_events, ["order"]);
        assert_eq!(halted.cap, Some(10_000));
        assert_eq!(halted.pointer.as_deref(), Some(""));
        assert_eq!(walked.final_state.status, "awaiting_conditions");
        assert_eq!(walked.final_state.node, "b");
    }

    #[test]
    fn a_capped_name_the_window_clears_is_still_decided() {
        let graph = graph(
            json!([
                {"id": "b", "type": "branch",
                 "conditions": [{"event": "order", "window": {"last": "P1D"}}]},
                {"id": "yes", "type": "exit"},
                {"id": "no", "type": "exit"}
            ]),
            &[("b", "case_0", "yes"), ("b", "else", "no")],
        );
        let mut snapshot = PersonSnapshot::default();
        snapshot.capped_events.insert("order".into());
        snapshot.events_complete_after = Some(to_time(at(-10 * 86_400)));

        let walked = run(&graph, snapshot, &[], MAX_STEPS).unwrap();

        assert!(walked.halted.is_none());
        assert_eq!(walked.path.last().unwrap().node, "no");
    }

    #[test]
    fn a_condition_the_workspace_does_not_know_stops_the_walk_as_invalid() {
        let graph = graph(
            json!([
                {"id": "b", "type": "branch",
                 "conditions": [{"trait": "nope", "op": "eq", "value": 1}]},
                {"id": "end", "type": "exit"}
            ]),
            &[("b", "case_0", "end"), ("b", "else", "end")],
        );

        let walked = run(&graph, PersonSnapshot::default(), &[], MAX_STEPS).unwrap();

        let halted = walked.halted.unwrap();
        assert_eq!(halted.reason, "invalid_condition");
        assert_eq!(halted.condition, Some(0));
        assert_eq!(walked.path[0].outcome, "failed");
    }

    #[test]
    fn the_person_time_zone_trait_decides_a_wait_until_time() {
        let graph = graph(
            json!([
                {"id": "w", "type": "wait.until_time",
                 "when": {"kind": "date_time", "at": "2030-06-15T12:00:00"}},
                {"id": "end", "type": "exit"}
            ]),
            &[("w", "next", "end")],
        );
        let mut snapshot = PersonSnapshot::default();
        snapshot
            .traits
            .insert("timezone".into(), json!("Asia/Tokyo"));

        let walked = run(&graph, snapshot, &[], MAX_STEPS).unwrap();

        // Noon in Tokyo is 03:00 UTC.
        assert_eq!(
            walked.final_state.wake_at,
            Some(time::macros::datetime!(2030-06-15 03:00:00 UTC))
        );
    }

    #[test]
    fn the_walk_refuses_more_steps_than_the_cap_and_a_script_that_goes_back() {
        let graph = journey();

        assert_eq!(
            run(
                &graph,
                PersonSnapshot::default(),
                &[Step::AdvanceTo(at(100))],
                1
            )
            .err(),
            Some(Refusal::StepLimit)
        );
        // The start, the timer and the answer to the branch are three steps.
        assert!(
            run(
                &graph,
                PersonSnapshot::default(),
                &[Step::AdvanceTo(at(100))],
                3
            )
            .is_ok()
        );
        assert_eq!(
            run(
                &graph,
                PersonSnapshot::default(),
                &[Step::AdvanceTo(at(100))],
                2
            )
            .err(),
            Some(Refusal::StepLimit)
        );
        assert_eq!(
            run(
                &graph,
                PersonSnapshot::default(),
                &[Step::AdvanceTo(at(100)), Step::AdvanceTo(at(50))],
                MAX_STEPS
            )
            .err(),
            Some(Refusal::ScriptOrder { index: 1 })
        );
        assert_eq!(
            run(
                &graph,
                PersonSnapshot::default(),
                &[Step::AdvanceTo(at(-1))],
                MAX_STEPS
            )
            .err(),
            Some(Refusal::ScriptOrder { index: 0 })
        );
    }

    #[test]
    fn an_event_that_wakes_nothing_is_recorded_and_steps_after_a_halt_are_skipped() {
        let recorded = run(
            &journey(),
            PersonSnapshot::default(),
            &[inject("visit", json!({}), 10)],
            MAX_STEPS,
        )
        .unwrap();
        assert_eq!(recorded.script[0].outcome, "event_recorded");

        let mut capped = PersonSnapshot::default();
        capped.capped_events.insert("order".into());
        let stopped = run(
            &capped_branch(),
            capped,
            &[Step::AdvanceTo(at(10))],
            MAX_STEPS,
        )
        .unwrap();
        assert!(stopped.halted.is_some());
        assert_eq!(stopped.script[0].outcome, "skipped");
    }
}
