//! The flow graph and its wire form (architecture section 9.1): nodes with stable ids, edges
//! typed by output port, and the node kinds of M5. Send and sync nodes arrive with M6.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::segment::{self, EventCondition, Node as Condition};

/// The graph version this build reads and writes.
pub const GRAPH_VERSION: u64 = 1;

/// The `type` of every node kind.
pub const NODE_TYPES: [&str; 10] = [
    "wait.duration",
    "wait.until_time",
    "wait.until_event",
    "branch",
    "split.random",
    "action.webhook",
    "action.update_trait",
    "action.list",
    "goal",
    "exit",
];

/// The port of a node with one way on.
pub const PORT_NEXT: &str = "next";
/// `wait.until_event`: the awaited event arrived.
pub const PORT_EVENT: &str = "event";
/// `wait.until_event`: the timeout passed first.
pub const PORT_TIMEOUT: &str = "timeout";
/// `branch`: no condition matched.
pub const PORT_ELSE: &str = "else";

/// The port of the `index`th condition of a `branch`.
pub fn case_port(index: usize) -> String {
    format!("case_{index}")
}

/// The port of the `index`th arm of a `split.random`.
pub fn arm_port(index: usize) -> String {
    format!("arm_{index}")
}

/// A flow graph: `{"v": 1, "nodes": [...], "edges": [...]}`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct FlowGraph {
    pub v: u64,
    pub nodes: Vec<FlowNode>,
    pub edges: Vec<Edge>,
}

/// One node: a stable id and its kind, flattened (`{"id": "n1", "type": "exit"}`).
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct FlowNode {
    pub id: String,
    #[serde(flatten)]
    pub kind: NodeKind,
}

/// One edge: the output port `port` of node `from` leads to node `to`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Edge {
    pub id: String,
    pub from: String,
    pub port: String,
    pub to: String,
}

/// What a node does.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type")]
pub enum NodeKind {
    /// Waits a fixed time.
    #[serde(rename = "wait.duration")]
    WaitDuration { seconds: u64 },
    /// Waits for a local time of day or a local date and time.
    #[serde(rename = "wait.until_time")]
    WaitUntilTime { when: UntilTime },
    /// Waits for an event, or the timeout. Ports `event` and `timeout`.
    #[serde(rename = "wait.until_event")]
    WaitUntilEvent {
        #[serde(flatten)]
        event: EventMatch,
        timeout_seconds: u64,
    },
    /// The first matching condition wins; ports `case_0`, `case_1`, ... and `else`.
    /// Each condition is a segment AST node in its wire form.
    #[serde(rename = "branch")]
    Branch { conditions: Vec<Value> },
    /// Sends a person down one arm by weight; ports `arm_0`, `arm_1`, ...
    #[serde(rename = "split.random")]
    SplitRandom { arms: Vec<Arm> },
    /// Calls a URL with the fixed envelope plus the listed trait keys.
    #[serde(rename = "action.webhook")]
    ActionWebhook {
        url: String,
        #[serde(default)]
        trait_keys: Vec<String>,
    },
    #[serde(rename = "action.update_trait")]
    ActionUpdateTrait { key: String, change: TraitChange },
    #[serde(rename = "action.list")]
    ActionList { list_id: Uuid, change: ListChange },
    /// Records a conversion; with `exit` it also ends the enrollment and has no port.
    #[serde(rename = "goal")]
    Goal {
        #[serde(default)]
        exit: bool,
    },
    #[serde(rename = "exit")]
    Exit,
}

impl NodeKind {
    /// The `type` on the wire; one of [`NODE_TYPES`].
    pub const fn type_name(&self) -> &'static str {
        match self {
            Self::WaitDuration { .. } => "wait.duration",
            Self::WaitUntilTime { .. } => "wait.until_time",
            Self::WaitUntilEvent { .. } => "wait.until_event",
            Self::Branch { .. } => "branch",
            Self::SplitRandom { .. } => "split.random",
            Self::ActionWebhook { .. } => "action.webhook",
            Self::ActionUpdateTrait { .. } => "action.update_trait",
            Self::ActionList { .. } => "action.list",
            Self::Goal { .. } => "goal",
            Self::Exit => "exit",
        }
    }

    /// The output ports of this node; every one must be wired exactly once.
    pub fn ports(&self) -> Vec<String> {
        match self {
            Self::WaitDuration { .. }
            | Self::WaitUntilTime { .. }
            | Self::ActionWebhook { .. }
            | Self::ActionUpdateTrait { .. }
            | Self::ActionList { .. }
            | Self::Goal { exit: false } => vec![PORT_NEXT.to_owned()],
            Self::WaitUntilEvent { .. } => vec![PORT_EVENT.to_owned(), PORT_TIMEOUT.to_owned()],
            Self::Branch { conditions } => (0..conditions.len())
                .map(case_port)
                .chain([PORT_ELSE.to_owned()])
                .collect(),
            Self::SplitRandom { arms } => (0..arms.len()).map(arm_port).collect(),
            Self::Goal { exit: true } | Self::Exit => Vec::new(),
        }
    }
}

/// An event name with property filters, the filter nodes of the segment AST in wire form
/// (`{"prop": "total", "op": "gt", "value": 100}`).
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct EventMatch {
    #[serde(rename = "event")]
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub filters: Vec<Value>,
}

impl EventMatch {
    /// The segment AST condition this matches on, or the segment issues that refuse it.
    pub fn to_condition(&self) -> Result<EventCondition, Vec<segment::Issue>> {
        let mut wire = json!({"v": 1, "event": self.name});
        if !self.filters.is_empty() {
            wire["where"] = Value::Array(self.filters.clone());
        }
        match segment::parse(&wire)?.root {
            Condition::Event(condition) => Ok(condition),
            _ => Err(vec![segment::Issue::new("", segment::code::MALFORMED)]),
        }
    }
}

/// `wait.until_time`: local to the person's time zone, else the workspace's.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind")]
pub enum UntilTime {
    /// The next `HH:MM`, on any of `weekdays` (every day when empty).
    #[serde(rename = "time_of_day")]
    TimeOfDay {
        time: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        weekdays: Vec<Weekday>,
    },
    /// A local `YYYY-MM-DDTHH:MM:SS`, with no offset.
    #[serde(rename = "date_time")]
    DateTime { at: String },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Weekday {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
    Sun,
}

/// One arm of a `split.random`; the weights of all arms sum to 100.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Arm {
    pub weight: u32,
}

/// `action.update_trait`: the flow's own contribution to a trait.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum TraitChange {
    Set { value: Value },
    Increment { by: f64 },
    Unset,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ListChange {
    Add,
    Remove,
}

impl FlowGraph {
    /// Parses the wire form. Content problems are the validator's, not this function's.
    pub fn from_value(value: Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(value)
    }

    pub fn node(&self, id: &str) -> Option<&FlowNode> {
        self.nodes.iter().find(|node| node.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event() -> EventMatch {
        EventMatch {
            name: "order".into(),
            filters: vec![json!({"prop": "total", "op": "gt", "value": 100})],
        }
    }

    /// At least one node of every kind, and every variant of the kinds that have them.
    fn every_kind() -> Vec<NodeKind> {
        vec![
            NodeKind::WaitDuration { seconds: 3600 },
            NodeKind::WaitUntilTime {
                when: UntilTime::TimeOfDay {
                    time: "09:30".into(),
                    weekdays: vec![Weekday::Mon, Weekday::Fri],
                },
            },
            NodeKind::WaitUntilTime {
                when: UntilTime::DateTime {
                    at: "2030-12-24T09:00:00".into(),
                },
            },
            NodeKind::WaitUntilEvent {
                event: event(),
                timeout_seconds: 86_400,
            },
            NodeKind::Branch {
                conditions: vec![json!({"trait": "plan", "op": "eq", "value": "pro"})],
            },
            NodeKind::SplitRandom {
                arms: vec![Arm { weight: 30 }, Arm { weight: 70 }],
            },
            NodeKind::ActionWebhook {
                url: "https://example.com/hook".into(),
                trait_keys: vec!["plan".into()],
            },
            NodeKind::ActionUpdateTrait {
                key: "plan".into(),
                change: TraitChange::Set {
                    value: json!("pro"),
                },
            },
            NodeKind::ActionUpdateTrait {
                key: "visits".into(),
                change: TraitChange::Increment { by: 2.5 },
            },
            NodeKind::ActionUpdateTrait {
                key: "plan".into(),
                change: TraitChange::Unset,
            },
            NodeKind::ActionList {
                list_id: Uuid::nil(),
                change: ListChange::Add,
            },
            NodeKind::ActionList {
                list_id: Uuid::nil(),
                change: ListChange::Remove,
            },
            NodeKind::Goal { exit: false },
            NodeKind::Goal { exit: true },
            NodeKind::Exit,
        ]
    }

    #[test]
    fn every_node_kind_round_trips_through_json() {
        let nodes: Vec<FlowNode> = every_kind()
            .into_iter()
            .enumerate()
            .map(|(index, kind)| FlowNode {
                id: format!("n{index}"),
                kind,
            })
            .collect();
        let graph = FlowGraph {
            v: GRAPH_VERSION,
            nodes,
            edges: vec![Edge {
                id: "e0".into(),
                from: "n0".into(),
                port: PORT_NEXT.into(),
                to: "n1".into(),
            }],
        };
        let wire = serde_json::to_value(&graph).unwrap();
        assert_eq!(FlowGraph::from_value(wire).unwrap(), graph);
        let text = serde_json::to_string(&graph).unwrap();
        assert_eq!(serde_json::from_str::<FlowGraph>(&text).unwrap(), graph);
    }

    #[test]
    fn the_wire_form_is_flat_and_tagged() {
        let node = FlowNode {
            id: "wait".into(),
            kind: NodeKind::WaitUntilEvent {
                event: event(),
                timeout_seconds: 60,
            },
        };
        assert_eq!(
            serde_json::to_value(&node).unwrap(),
            json!({
                "id": "wait",
                "type": "wait.until_event",
                "event": "order",
                "filters": [{"prop": "total", "op": "gt", "value": 100}],
                "timeout_seconds": 60,
            })
        );
        let edge: Edge =
            serde_json::from_value(json!({"id": "e", "from": "a", "port": "next", "to": "b"}))
                .unwrap();
        assert_eq!(edge.port, PORT_NEXT);
    }

    #[test]
    fn every_kind_reports_its_type_and_ports() {
        let kinds = every_kind();
        let types: Vec<&str> = kinds.iter().map(NodeKind::type_name).collect();
        for name in NODE_TYPES {
            assert!(types.contains(&name), "{name} has no example");
        }
        for name in &types {
            assert!(NODE_TYPES.contains(name), "{name} is not listed");
        }
        assert_eq!(kinds[0].ports(), ["next"]);
        assert_eq!(kinds[3].ports(), ["event", "timeout"]);
        assert_eq!(kinds[4].ports(), ["case_0", "else"]);
        assert_eq!(kinds[5].ports(), ["arm_0", "arm_1"]);
        assert_eq!(NodeKind::Goal { exit: false }.ports(), ["next"]);
        assert!(NodeKind::Goal { exit: true }.ports().is_empty());
        assert!(NodeKind::Exit.ports().is_empty());
    }

    #[test]
    fn an_event_match_becomes_a_segment_event_condition() {
        let condition = event().to_condition().unwrap();
        assert_eq!(condition.name, "order");
        assert_eq!(condition.filters.len(), 1);
        let bad = EventMatch {
            name: "order".into(),
            filters: vec![json!({"prop": "total", "op": "nope"})],
        };
        assert!(bad.to_condition().is_err());
    }
}
