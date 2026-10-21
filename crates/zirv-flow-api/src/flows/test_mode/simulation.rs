//! The simulation summary of an assistant's flow draft (architecture 9.4, 13.2): test-mode walks of
//! a few synthetic people through the candidate graph, run through the same walk the `test`
//! endpoint runs, and reduced to what a reviewer needs to see before accepting: which nodes each
//! person reaches, where each one ends, and which nodes nobody reaches.
//!
//! It is pure over its inputs: the people are built from the graph itself (no person is read, so
//! no personal value is involved), the clock is virtual, and nothing is executed or written.

use chrono::{DateTime, Duration, Utc};
use chrono_tz::Tz;
use serde::Serialize;
use serde_json::{Map, Value};
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::flow::{FlowGraph, FlowSettings, NodeKind};
use zirv_flow_segments::{Catalog, PersonSnapshot, SnapshotEvent};
use zirv_flow_store::EXPLAIN_EVENT_CAP;

use super::walk::{
    MAX_STEPS, Refusal, Step, Subject, Walk, Walked, to_chrono, to_time, walk,
};

/// How far past the start the virtual clock is advanced, so every timer of a flow fires: a single
/// wait is at most a year.
const WINDOW_DAYS: i64 = 400;
/// The most events one synthetic person gets for a count condition.
const MAX_SYNTHETIC_EVENTS: u64 = 5;

/// One synthetic person's walk, reduced.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, ToSchema)]
pub(crate) struct SimulatedPersonBody {
    /// `blank`, `first_case` or `last_case`.
    label: &'static str,
    /// Who the synthetic person is, in plain text.
    description: &'static str,
    /// The ids of the nodes the person ran, in order.
    path: Vec<String>,
    /// How the walk ended: `completed`, `exited`, `waiting`, `failed` and so on; null when the
    /// walk was refused.
    final_status: Option<String>,
    /// The node the walk ended at; null when it was refused.
    final_node: Option<String>,
    /// Why the walk stopped before the end, when it did, such as `unable_to_evaluate`.
    halted: Option<String>,
    /// How many changes of the person (a trait, a list) the walk made.
    internal_effects: usize,
    /// How many outward calls (a webhook) the flow would make for the person. None was made.
    external_effects: usize,
    /// Set when the walk could not be run: `step_limit` or `no_entry`.
    refused: Option<&'static str>,
}

/// What a few synthetic people do in the candidate flow.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, ToSchema)]
pub(crate) struct SimulationBody {
    /// The virtual time every walk started at.
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    start_at: OffsetDateTime,
    people: Vec<SimulatedPersonBody>,
    /// The nodes none of the synthetic people reached.
    unreached_nodes: Vec<String>,
    /// What a walk cannot do, as stable codes (such as `goal_segment`).
    not_simulated: Vec<&'static str>,
}

/// Which branch condition a synthetic person meets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Meets {
    Nothing,
    First,
    Last,
}

const PEOPLE: [(&str, &str, Meets); 3] = [
    ("blank", "A person with nothing on file.", Meets::Nothing),
    (
        "first_case",
        "A person who meets the first condition of every branch.",
        Meets::First,
    ),
    (
        "last_case",
        "A person who meets the last condition of every branch.",
        Meets::Last,
    ),
];

/// Makes `snapshot` meet `condition` as far as a simple person can. A condition it does not know
/// how to meet is left alone; the walk then shows the person failing it. `last` picks the last
/// alternative of an `any`.
fn meet(condition: &Value, last: bool, start: DateTime<Utc>, snapshot: &mut PersonSnapshot) {
    let Some(map) = condition.as_object() else {
        return;
    };
    if let Some(children) = map.get("all").and_then(Value::as_array) {
        for child in children {
            meet(child, last, start, snapshot);
        }
    } else if let Some(children) = map.get("any").and_then(Value::as_array) {
        let pick = if last { children.last() } else { children.first() };
        if let Some(child) = pick {
            meet(child, last, start, snapshot);
        }
    } else if let Some(key) = map.get("trait").and_then(Value::as_str) {
        if let Some(value) = trait_value(map) {
            snapshot.traits.insert(key.to_owned(), value);
        }
    } else if let Some(id) = map
        .get("list")
        .and_then(Value::as_str)
        .and_then(|id| id.parse::<Uuid>().ok())
    {
        snapshot.lists.insert(id);
    } else if let Some(id) = map
        .get("segment")
        .and_then(Value::as_str)
        .and_then(|id| id.parse::<Uuid>().ok())
    {
        snapshot.segments.insert(id);
    } else if let Some(kind) = map.get("identifier").and_then(Value::as_str) {
        snapshot.identifier_kinds.insert(kind.to_owned());
    } else if let Some(name) = map.get("event").and_then(Value::as_str) {
        let mut properties = Map::new();
        for filter in map
            .get("where")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if filter.get("op").and_then(Value::as_str) == Some("eq")
                && let (Some(prop), Some(value)) =
                    (filter.get("prop").and_then(Value::as_str), filter.get("value"))
            {
                properties.insert(prop.to_owned(), value.clone());
            }
        }
        let count = map
            .get("agg")
            .filter(|agg| agg.get("fn").and_then(Value::as_str) == Some("count"))
            .and_then(|agg| agg.get("value").and_then(Value::as_u64))
            .unwrap_or(1)
            .clamp(1, MAX_SYNTHETIC_EVENTS);
        for index in 0..count {
            snapshot.events.push(SnapshotEvent {
                name: name.to_owned(),
                occurred_at: to_time(start - Duration::hours(1 + i64::try_from(index).unwrap_or(0))),
                properties: properties.clone(),
            });
        }
    }
}

/// A value for a trait that makes the condition on it hold, for the operators that have one.
fn trait_value(condition: &Map<String, Value>) -> Option<Value> {
    let value = condition.get("value");
    match condition.get("op").and_then(Value::as_str)? {
        "eq" | "contains" | "starts_with" => value.cloned(),
        "in" => value.and_then(Value::as_array).and_then(|items| items.first().cloned()),
        "gt" | "gte" => value
            .and_then(Value::as_f64)
            .and_then(|number| serde_json::Number::from_f64(number + 1.0))
            .map(Value::Number),
        "lt" | "lte" => value
            .and_then(Value::as_f64)
            .and_then(|number| serde_json::Number::from_f64(number - 1.0))
            .map(Value::Number),
        "exists" => Some(Value::String("x".to_owned())),
        _ => None,
    }
}

/// The synthetic person who meets the branch conditions of `graph` as `meets` says.
fn person(graph: &FlowGraph, meets: Meets, start: DateTime<Utc>) -> PersonSnapshot {
    let mut snapshot = PersonSnapshot::default();
    if meets == Meets::Nothing {
        return snapshot;
    }
    for node in &graph.nodes {
        let NodeKind::Branch { conditions } = &node.kind else {
            continue;
        };
        let pick = if meets == Meets::Last {
            conditions.last()
        } else {
            conditions.first()
        };
        if let Some(condition) = pick {
            meet(condition, meets == Meets::Last, start, &mut snapshot);
        }
    }
    snapshot
}

fn reduced(
    label: &'static str,
    description: &'static str,
    outcome: Result<Walked, Refusal>,
) -> (SimulatedPersonBody, Vec<&'static str>) {
    match outcome {
        Ok(walked) => {
            let not_simulated = walked.not_simulated.clone();
            let mut path: Vec<String> = Vec::new();
            for step in &walked.path {
                if !path.contains(&step.node) {
                    path.push(step.node.clone());
                }
            }
            (
                SimulatedPersonBody {
                    label,
                    description,
                    path,
                    final_status: Some(walked.final_state.status.clone()),
                    final_node: Some(walked.final_state.node.clone()),
                    halted: walked.halted.as_ref().map(|halt| halt.reason.to_owned()),
                    internal_effects: walked.internal_effects.len(),
                    external_effects: walked.external_effects.len(),
                    refused: None,
                },
                not_simulated,
            )
        }
        Err(refusal) => (
            SimulatedPersonBody {
                label,
                description,
                path: Vec::new(),
                final_status: None,
                final_node: None,
                halted: None,
                internal_effects: 0,
                external_effects: 0,
                refused: Some(match refusal {
                    Refusal::StepLimit => "step_limit",
                    Refusal::ScriptOrder { .. } => "script_order",
                    Refusal::NoEntry => "no_entry",
                }),
            },
            Vec::new(),
        ),
    }
}

/// Walks the synthetic people through a graph that passed the publish validation, from `start`.
/// Nothing is read from the database and nothing is written.
pub(crate) fn simulate(
    graph: &FlowGraph,
    settings: &FlowSettings,
    catalog: &Catalog,
    zone: Tz,
    flow_id: Uuid,
    start: OffsetDateTime,
) -> SimulationBody {
    let start_at = to_chrono(start).unwrap_or_default();
    let script = [Step::AdvanceTo(start_at + Duration::days(WINDOW_DAYS))];
    let mut people = Vec::new();
    let mut not_simulated: Vec<&'static str> = Vec::new();
    for (index, (label, description, meets)) in PEOPLE.into_iter().enumerate() {
        let subject = Subject {
            flow_id,
            version: None,
            person_id: None,
            enrollment_id: Uuid::from_u128(flow_id.as_u128() ^ (index as u128 + 1)),
        };
        let outcome = walk(Walk {
            graph,
            settings,
            catalog,
            subject: &subject,
            zone,
            snapshot: person(graph, meets, start_at),
            start_at,
            script: &script,
            max_steps: MAX_STEPS,
            event_cap: EXPLAIN_EVENT_CAP,
        });
        let (body, codes) = reduced(label, description, outcome);
        for code in codes {
            if !not_simulated.contains(&code) {
                not_simulated.push(code);
            }
        }
        people.push(body);
    }
    let unreached_nodes = graph
        .nodes
        .iter()
        .filter(|node| !people.iter().any(|body| body.path.contains(&node.id)))
        .map(|node| node.id.clone())
        .collect();
    SimulationBody {
        start_at: start,
        people,
        unreached_nodes,
        not_simulated,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use time::macros::datetime;
    use zirv_flow_core::people::TraitType;

    use super::*;

    const START: OffsetDateTime = datetime!(2026-05-15 12:00 UTC);

    fn catalog() -> Catalog {
        let mut catalog = Catalog::default();
        catalog.traits.insert("plan".into(), TraitType::String);
        catalog.traits.insert("visits".into(), TraitType::Number);
        catalog
    }

    fn settings() -> FlowSettings {
        serde_json::from_value(json!({"trigger": {"type": "manual"}, "reentry": {"mode": "once"}}))
            .unwrap()
    }

    /// Wait a day, then branch on the plan or on visits; webhook for the first case.
    fn graph() -> FlowGraph {
        FlowGraph::from_value(json!({"v": 1, "nodes": [
            {"id": "wait", "type": "wait.duration", "seconds": 86400},
            {"id": "check", "type": "branch", "conditions": [
                {"trait": "plan", "op": "eq", "value": "pro"},
                {"trait": "visits", "op": "gte", "value": 3}
            ]},
            {"id": "pro", "type": "action.update_trait", "key": "plan",
             "change": {"op": "set", "value": "vip"}},
            {"id": "visitor", "type": "exit"},
            {"id": "other", "type": "exit"},
            {"id": "end", "type": "exit"}
        ], "edges": [
            {"id": "e1", "from": "wait", "port": "next", "to": "check"},
            {"id": "e2", "from": "check", "port": "case_0", "to": "pro"},
            {"id": "e3", "from": "check", "port": "case_1", "to": "visitor"},
            {"id": "e4", "from": "check", "port": "else", "to": "other"},
            {"id": "e5", "from": "pro", "port": "next", "to": "end"}
        ]}))
        .unwrap()
    }

    fn run() -> SimulationBody {
        simulate(&graph(), &settings(), &catalog(), Tz::UTC, Uuid::nil(), START)
    }

    #[test]
    fn each_synthetic_person_takes_the_branch_they_were_made_for() {
        let simulation = run();

        let paths: Vec<(&str, Vec<&str>)> = simulation
            .people
            .iter()
            .map(|body| {
                (
                    body.label,
                    body.path.iter().map(String::as_str).collect::<Vec<_>>(),
                )
            })
            .collect();
        assert_eq!(
            paths,
            [
                ("blank", vec!["wait", "check", "other"]),
                ("first_case", vec!["wait", "check", "pro", "end"]),
                ("last_case", vec!["wait", "check", "visitor"]),
            ]
        );
        assert!(simulation.unreached_nodes.is_empty());
        assert_eq!(simulation.people[1].internal_effects, 1);
        assert_eq!(simulation.people[1].final_status.as_deref(), Some("completed"));
        assert_eq!(simulation.start_at, START);
    }

    #[test]
    fn a_replay_is_identical_and_the_blank_person_takes_the_else_port() {
        let one = FlowGraph::from_value(json!({"v": 1, "nodes": [
            {"id": "b", "type": "branch", "conditions": [{"trait": "plan", "op": "eq", "value": "pro"}]},
            {"id": "yes", "type": "exit"},
            {"id": "no", "type": "exit"}
        ], "edges": [
            {"id": "e1", "from": "b", "port": "case_0", "to": "yes"},
            {"id": "e2", "from": "b", "port": "else", "to": "no"}
        ]}))
        .unwrap();

        let once = simulate(&one, &settings(), &catalog(), Tz::UTC, Uuid::nil(), START);
        let again = simulate(&one, &settings(), &catalog(), Tz::UTC, Uuid::nil(), START);

        assert_eq!(once, again);
        let blank: Vec<&str> = once.people[0].path.iter().map(String::as_str).collect();
        assert_eq!(blank, ["b", "no"]);
        let serialized = serde_json::to_value(&once).unwrap();
        assert_eq!(serialized["people"][0]["label"], "blank");
        assert_eq!(serialized["start_at"], "2026-05-15T12:00:00Z");
    }

    #[test]
    fn a_node_nobody_reaches_is_listed() {
        // No synthetic person does the event the wait is for, so its event port is never taken.
        let waiting = FlowGraph::from_value(json!({"v": 1, "nodes": [
            {"id": "wait", "type": "wait.until_event", "event": "order", "timeout_seconds": 3600},
            {"id": "got", "type": "exit"},
            {"id": "late", "type": "exit"}
        ], "edges": [
            {"id": "e1", "from": "wait", "port": "event", "to": "got"},
            {"id": "e2", "from": "wait", "port": "timeout", "to": "late"}
        ]}))
        .unwrap();

        let simulation = simulate(&waiting, &settings(), &catalog(), Tz::UTC, Uuid::nil(), START);

        assert_eq!(simulation.unreached_nodes, ["got"]);
        assert!(simulation.people.iter().all(|body| body.path.contains(&"late".to_owned())));
    }

    #[test]
    fn a_graph_without_an_entry_is_refused_not_walked() {
        let cycle = FlowGraph::from_value(json!({"v": 1, "nodes": [
            {"id": "a", "type": "wait.duration", "seconds": 60},
            {"id": "b", "type": "wait.duration", "seconds": 60}
        ], "edges": [
            {"id": "e1", "from": "a", "port": "next", "to": "b"},
            {"id": "e2", "from": "b", "port": "next", "to": "a"}
        ]}))
        .unwrap();

        let simulation = simulate(&cycle, &settings(), &catalog(), Tz::UTC, Uuid::nil(), START);

        assert!(simulation.people.iter().all(|body| body.refused == Some("no_entry")));
        assert_eq!(simulation.unreached_nodes, ["a", "b"]);
    }

    #[test]
    fn conditions_are_met_by_the_values_that_make_them_hold() {
        let start = to_chrono(START).unwrap();
        let mut snapshot = PersonSnapshot::default();
        let list = Uuid::now_v7();

        for condition in [
            json!({"trait": "city", "op": "in", "value": ["Aarhus", "Odense"]}),
            json!({"trait": "visits", "op": "lt", "value": 3}),
            json!({"list": list.to_string()}),
            json!({"identifier": "email"}),
            json!({"event": "order", "where": [{"prop": "kind", "op": "eq", "value": "gift"}],
                   "agg": {"fn": "count", "op": "gte", "value": 2}}),
            json!({"not": {"trait": "ghost", "op": "eq", "value": "x"}}),
        ] {
            meet(&condition, false, start, &mut snapshot);
        }

        assert_eq!(snapshot.traits["city"], "Aarhus");
        assert_eq!(snapshot.traits["visits"], json!(2.0));
        assert!(!snapshot.traits.contains_key("ghost"));
        assert!(snapshot.lists.contains(&list));
        assert!(snapshot.identifier_kinds.contains("email"));
        assert_eq!(snapshot.events.len(), 2);
        assert_eq!(snapshot.events[0].properties["kind"], "gift");
    }
}
