//! Seeded flow graphs. Every graph is built acyclic by construction (an edge only leads to a
//! node created after its source), every node is reached through the edge that created it, every
//! port is wired once, and the result is asserted to pass `core::flow::validate_graph`.

use std::collections::BTreeMap;

use chrono::{Days, NaiveDate};
use serde_json::json;
use uuid::Uuid;
use zirv_flow_core::flow::{
    Arm, Edge, EventMatch, FlowGraph, FlowNode, GRAPH_VERSION, ListChange, NodeKind, SyncOperation,
    TraitChange, UntilTime, Weekday, validate_graph,
};

use super::rng::Rng;

/// The events the generated waits listen for.
pub const EVENTS: [&str; 3] = ["order", "visit", "signup"];

/// One shape of node. `Goal` appears twice because the two forms differ in their ports.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Shape {
    WaitDuration,
    WaitTimeOfDay,
    WaitDateTime,
    WaitEvent,
    Branch,
    Split,
    Webhook,
    Send,
    Sync,
    SetTrait,
    IncrementTrait,
    UnsetTrait,
    List,
    GoalContinue,
    GoalExit,
    Exit,
}

/// The shapes with output ports that random graphs draw from: every kind that is not an end and
/// not in [`COVERED_ONLY`].
const OPEN: [Shape; 12] = [
    Shape::WaitDuration,
    Shape::WaitTimeOfDay,
    Shape::WaitDateTime,
    Shape::WaitEvent,
    Shape::Branch,
    Shape::Split,
    Shape::Webhook,
    Shape::SetTrait,
    Shape::IncrementTrait,
    Shape::UnsetTrait,
    Shape::List,
    Shape::GoalContinue,
];

/// The shapes with output ports that only a covering graph holds. `action.send` is here, not in
/// [`OPEN`], so the seeded populations of the other kinds draw the same graphs they did before
/// the send node existed, and each covering graph still has one. `action.sync` is here for the
/// same reason.
const COVERED_ONLY: [Shape; 2] = [Shape::Send, Shape::Sync];

/// The shapes that end an enrollment.
const ENDS: [Shape; 2] = [Shape::GoalExit, Shape::Exit];

const WEEKDAYS: [Weekday; 7] = [
    Weekday::Mon,
    Weekday::Tue,
    Weekday::Wed,
    Weekday::Thu,
    Weekday::Fri,
    Weekday::Sat,
    Weekday::Sun,
];

/// Local times around the 2030 clock changes of the zones the population lives in.
const TRICKY_LOCAL_TIMES: [&str; 4] = [
    "2030-03-31T02:30:00",
    "2030-10-27T02:30:00",
    "2030-03-10T02:30:00",
    "2030-11-03T01:30:00",
];

/// The id of the `index`th list a generated `action.list` node names (`index` is below four).
pub const fn list_id(index: u128) -> Uuid {
    Uuid::from_u128(LIST_BASE + index)
}

const LIST_BASE: u128 = 0x018f_0000_0000_7000_8000_0000_0000_0010;

/// The id of a template, destination or topic a generated `action.send` node names.
pub const fn send_id(index: u128) -> Uuid {
    Uuid::from_u128(SEND_BASE + index)
}

const SEND_BASE: u128 = 0x018f_0000_0000_7000_8000_0000_0000_0100;

fn kind(rng: &mut Rng, shape: Shape) -> NodeKind {
    match shape {
        Shape::WaitDuration => NodeKind::WaitDuration {
            seconds: rng.range(60, 5 * 86_400),
        },
        Shape::WaitTimeOfDay => {
            let mut weekdays = Vec::new();
            if rng.chance(50) {
                for day in WEEKDAYS {
                    if rng.chance(40) {
                        weekdays.push(day);
                    }
                }
            }
            NodeKind::WaitUntilTime {
                when: UntilTime::TimeOfDay {
                    time: format!("{:02}:{:02}", rng.below(24), rng.below(60)),
                    weekdays,
                },
            }
        }
        Shape::WaitDateTime => {
            let at = if rng.chance(15) {
                (*rng.pick(&TRICKY_LOCAL_TIMES)).to_owned()
            } else {
                let day = NaiveDate::from_ymd_opt(2030, 1, 1)
                    .and_then(|start| start.checked_add_days(Days::new(rng.range(0, 150))))
                    .unwrap_or_default();
                format!(
                    "{}T{:02}:{:02}:00",
                    day.format("%Y-%m-%d"),
                    rng.below(24),
                    rng.below(60)
                )
            };
            NodeKind::WaitUntilTime {
                when: UntilTime::DateTime { at },
            }
        }
        Shape::WaitEvent => NodeKind::WaitUntilEvent {
            event: EventMatch {
                name: (*rng.pick(&EVENTS)).to_owned(),
                filters: if rng.chance(40) {
                    vec![json!({"prop": "total", "op": "gt", "value": rng.below(500)})]
                } else {
                    Vec::new()
                },
            },
            timeout_seconds: rng.range(3_600, 10 * 86_400),
        },
        Shape::Branch => {
            let plans = ["free", "pro", "team", "trial"];
            let conditions = (0..rng.range(1, 4))
                .map(|_| json!({"trait": "plan", "op": "eq", "value": *rng.pick(&plans)}))
                .collect();
            NodeKind::Branch { conditions }
        }
        Shape::Split => NodeKind::SplitRandom {
            arms: {
                let count = usize::try_from(rng.range(2, 4)).unwrap_or(2);
                arms(rng, count)
            },
        },
        Shape::Webhook => {
            let keys = ["plan", "country", "age", "vip"];
            let trait_keys = keys
                .iter()
                .filter(|_| rng.chance(40))
                .map(|key| (*key).to_owned())
                .collect();
            NodeKind::ActionWebhook {
                url: format!("https://hooks.example.com/{}", rng.below(1000)),
                trait_keys,
                template: None,
            }
        }
        Shape::Send => NodeKind::ActionSend {
            template: send_id(u128::from(rng.below(3))),
            destination: send_id(10 + u128::from(rng.below(2))),
            topic: rng.chance(50).then(|| send_id(20)),
            dimensions: if rng.chance(30) {
                [("vertical".to_owned(), "sports".to_owned())].into()
            } else {
                BTreeMap::new()
            },
        },
        Shape::Sync => {
            let remove = rng.chance(40);
            let keys = ["plan", "country", "age"];
            let trait_keys = keys
                .iter()
                .filter(|_| !remove && rng.chance(50))
                .map(|key| (*key).to_owned())
                .collect();
            NodeKind::ActionSync {
                destination: send_id(30 + u128::from(rng.below(2))),
                operation: if remove {
                    SyncOperation::Remove
                } else {
                    SyncOperation::Upsert
                },
                trait_keys,
            }
        }
        Shape::SetTrait => NodeKind::ActionUpdateTrait {
            key: format!("t{}", rng.below(5)),
            change: TraitChange::Set {
                value: json!(rng.below(100)),
            },
        },
        Shape::IncrementTrait => NodeKind::ActionUpdateTrait {
            key: format!("t{}", rng.below(5)),
            change: TraitChange::Increment {
                by: f64::from(u32::try_from(rng.below(10)).unwrap_or(1)),
            },
        },
        Shape::UnsetTrait => NodeKind::ActionUpdateTrait {
            key: format!("t{}", rng.below(5)),
            change: TraitChange::Unset {},
        },
        Shape::List => NodeKind::ActionList {
            list_id: list_id(u128::from(rng.below(4))),
            change: if rng.chance(70) {
                ListChange::Add
            } else {
                ListChange::Remove
            },
        },
        Shape::GoalContinue => NodeKind::Goal { exit: false },
        Shape::GoalExit => NodeKind::Goal { exit: true },
        Shape::Exit => NodeKind::Exit {},
    }
}

/// `count` arms with positive weights that sum to 100: cut points between 1 and 99.
fn arms(rng: &mut Rng, count: usize) -> Vec<Arm> {
    let mut cuts: Vec<u64> = Vec::new();
    while cuts.len() + 1 < count {
        let cut = rng.range(1, 99);
        if !cuts.contains(&cut) {
            cuts.push(cut);
        }
    }
    cuts.sort_unstable();
    cuts.push(100);
    let mut previous = 0;
    cuts.into_iter()
        .map(|cut| {
            let weight = u32::try_from(cut - previous).unwrap_or(1);
            previous = cut;
            Arm { weight }
        })
        .collect()
}

/// The graph under construction: nodes, edges and the ports not wired yet.
struct Build {
    nodes: Vec<FlowNode>,
    /// Ports not wired yet, as (node index, port).
    open: Vec<(usize, String)>,
}

impl Build {
    fn add(&mut self, rng: &mut Rng, shape: Shape) -> usize {
        let index = self.nodes.len();
        let node = FlowNode {
            id: format!("n{index}"),
            kind: kind(rng, shape),
        };
        self.open
            .extend(node.kind.ports().into_iter().map(|port| (index, port)));
        self.nodes.push(node);
        index
    }
}

/// A graph of about `size` nodes. With `cover_all` it holds every node kind, in a random order.
pub fn graph(rng: &mut Rng, size: usize, cover_all: bool) -> FlowGraph {
    // The shapes still owed to the graph: the open ones first, then the ends.
    let mut owed: Vec<Shape> = Vec::new();
    let size = if cover_all {
        owed.extend(OPEN);
        owed.extend(COVERED_ONLY);
        rng.shuffle(&mut owed);
        owed.extend(ENDS);
        size.max(OPEN.len() + COVERED_ONLY.len() + 4)
    } else {
        size.max(2)
    };

    let mut build = Build {
        nodes: Vec::new(),
        open: Vec::new(),
    };
    let mut edges: Vec<Edge> = Vec::new();
    // The shared ends, made on demand once the size is reached.
    let mut ends: Vec<usize> = Vec::new();

    // The entry always has ports.
    let entry = if owed.first().is_some_and(|shape| !ENDS.contains(shape)) {
        owed.remove(0)
    } else {
        *rng.pick(&OPEN)
    };
    build.add(rng, entry);

    while !build.open.is_empty() {
        let slot = rng.index(build.open.len());
        let (from, port) = build.open.swap_remove(slot);
        let to = if let Some(shape) = take_owed(&mut owed) {
            build.add(rng, shape)
        } else if build.nodes.len() >= size {
            // Out of budget: lead to an end, shared by every port that needs one. Only an end
            // made after this node can be its target (an acyclic edge).
            let later: Vec<usize> = ends.iter().copied().filter(|end| *end > from).collect();
            if later.is_empty() || rng.chance(15) {
                let shape = *rng.pick(&ENDS);
                let end = build.add(rng, shape);
                ends.push(end);
                end
            } else {
                *rng.pick(&later)
            }
        } else if from + 1 < build.nodes.len() && rng.chance(25) {
            // Join an existing later node: a diamond, not a cycle.
            from + 1 + rng.index(build.nodes.len() - from - 1)
        } else {
            let pool: &[Shape] = if rng.chance(8) { &ENDS } else { &OPEN };
            let shape = *rng.pick(pool);
            build.add(rng, shape)
        };
        edges.push(Edge {
            id: format!("e{}", edges.len()),
            from: build.nodes[from].id.clone(),
            port,
            to: build.nodes[to].id.clone(),
        });
    }

    let graph = FlowGraph {
        v: GRAPH_VERSION,
        nodes: build.nodes,
        edges,
    };
    let issues = validate_graph(&graph);
    assert!(
        issues.is_empty(),
        "the generator made an invalid graph: {issues:?}"
    );
    graph
}

/// The next owed shape: an open one while any is owed, so the ports do not run dry before the
/// ends are placed.
fn take_owed(owed: &mut Vec<Shape>) -> Option<Shape> {
    let position = owed
        .iter()
        .position(|shape| !ENDS.contains(shape))
        .or_else(|| (!owed.is_empty()).then_some(0))?;
    Some(owed.remove(position))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The node `type`s of a graph, sorted and without repeats.
    fn kinds_of(graph: &FlowGraph) -> Vec<&'static str> {
        let mut kinds: Vec<&'static str> = graph
            .nodes
            .iter()
            .map(|node| node.kind.type_name())
            .collect();
        kinds.sort_unstable();
        kinds.dedup();
        kinds
    }

    #[test]
    fn generated_graphs_validate_across_seeds_and_sizes() {
        for seed in 0..60 {
            let mut rng = Rng::new(seed);
            for size in [2, 5, 12, 40] {
                // `graph` asserts validity itself; this also pins that it never panics.
                let made = graph(&mut rng, size, seed % 3 == 0);
                assert!(!made.nodes.is_empty());
            }
        }
    }

    #[test]
    fn a_covering_graph_has_every_node_kind() {
        let mut expected: Vec<&str> = zirv_flow_core::flow::NODE_TYPES.to_vec();
        expected.sort_unstable();
        for seed in 0..20 {
            let made = graph(&mut Rng::new(seed), 20, true);
            assert_eq!(kinds_of(&made), expected, "seed {seed}");
            assert!(
                made.nodes
                    .iter()
                    .any(|node| matches!(node.kind, NodeKind::Goal { exit: true }))
                    && made
                        .nodes
                        .iter()
                        .any(|node| matches!(node.kind, NodeKind::Goal { exit: false }))
            );
        }
    }

    #[test]
    fn the_same_seed_makes_the_same_graph() {
        let a = graph(&mut Rng::new(5), 25, false);
        let b = graph(&mut Rng::new(5), 25, false);
        assert_eq!(a, b);
    }
}
