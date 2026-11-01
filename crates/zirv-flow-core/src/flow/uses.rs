//! Where a flow version names a segment: the places the publish checks walk, read without the
//! catalog. A draft may be incomplete, so this reads the wire values and judges nothing.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use uuid::Uuid;

use super::graph::{EventMatch, FlowGraph, NodeKind};
use super::settings::{Audience, ExitCriterion, FlowSettings, Goal, Trigger};

/// How a flow version uses a segment.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SegmentUse {
    /// The trigger is entering or leaving the segment.
    Trigger,
    /// The segment narrows who a trigger admits: a schedule's audience, or a segment node in an
    /// event trigger's filters.
    Filter,
    /// A segment node in a node's condition: a branch, a wait-until-event match.
    Condition,
    /// Entering the segment is the flow's conversion goal.
    Goal,
}

impl SegmentUse {
    /// The word on the wire.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trigger => "trigger",
            Self::Filter => "filter",
            Self::Condition => "condition",
            Self::Goal => "goal",
        }
    }
}

/// The segments a version's graph and settings name, each with every way it is used.
pub fn segment_uses(
    graph: &FlowGraph,
    settings: &FlowSettings,
) -> BTreeMap<Uuid, BTreeSet<SegmentUse>> {
    let mut found = Found::default();
    match &settings.trigger {
        Trigger::Segment { segment_id, .. } => found.add(*segment_id, SegmentUse::Trigger),
        Trigger::Event(event) => found.event(event, SegmentUse::Filter),
        Trigger::Schedule {
            audience: Audience::Segment { segment_id },
            ..
        } => found.add(*segment_id, SegmentUse::Filter),
        Trigger::Schedule { .. } | Trigger::Manual {} | Trigger::Api {} => {}
    }
    match &settings.goal {
        Some(Goal::Segment { segment_id }) => found.add(*segment_id, SegmentUse::Goal),
        Some(Goal::Event(event)) => found.event(event, SegmentUse::Filter),
        None => {}
    }
    for criterion in &settings.exit_criteria {
        if let ExitCriterion::Event(event) = criterion {
            found.event(event, SegmentUse::Filter);
        }
    }
    for node in &graph.nodes {
        match &node.kind {
            NodeKind::WaitUntilEvent { event, .. } => found.event(event, SegmentUse::Condition),
            NodeKind::Branch { conditions } => {
                for condition in conditions {
                    found.condition(condition, SegmentUse::Condition);
                }
            }
            _ => {}
        }
    }
    found.0
}

#[derive(Default)]
struct Found(BTreeMap<Uuid, BTreeSet<SegmentUse>>);

impl Found {
    fn add(&mut self, id: Uuid, how: SegmentUse) {
        self.0.entry(id).or_default().insert(how);
    }

    fn event(&mut self, event: &EventMatch, how: SegmentUse) {
        for filter in &event.filters {
            self.condition(filter, how);
        }
    }

    /// A condition in its wire form: `{"segment": "<id>"}` leaves, inside `all`, `any` and `not`.
    fn condition(&mut self, condition: &Value, how: SegmentUse) {
        let Some(map) = condition.as_object() else {
            return;
        };
        if let Some(id) = map
            .get("segment")
            .and_then(Value::as_str)
            .and_then(|id| id.parse().ok())
        {
            self.add(id, how);
        }
        for key in ["all", "any"] {
            for child in map.get(key).and_then(Value::as_array).into_iter().flatten() {
                self.condition(child, how);
            }
        }
        if let Some(child) = map.get("not") {
            self.condition(child, how);
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn graph(nodes: Value) -> FlowGraph {
        serde_json::from_value(json!({"v": 1, "nodes": nodes, "edges": []})).unwrap()
    }

    fn settings(value: Value) -> FlowSettings {
        serde_json::from_value(value).unwrap()
    }

    fn uses(of: &BTreeMap<Uuid, BTreeSet<SegmentUse>>, segment: u128) -> Vec<&'static str> {
        of.get(&id(segment))
            .map(|set| set.iter().map(|how| how.as_str()).collect())
            .unwrap_or_default()
    }

    #[test]
    fn one_segment_used_every_way_lists_every_use_once() {
        let seg = id(1).to_string();
        let graph = graph(json!([
            {"id": "b", "type": "branch", "conditions": [
                {"all": [{"trait": "plan", "op": "eq", "value": "pro"},
                         {"not": {"segment": seg}}]}]},
            {"id": "w", "type": "wait.until_event", "event": "paid", "timeout_seconds": 60,
             "filters": [{"segment": seg}]},
        ]));
        let settings = settings(json!({
            "trigger": {"type": "segment", "segment_id": seg, "on": "entered"},
            "reentry": {"mode": "once"},
            "goal": {"type": "segment", "segment_id": seg},
        }));

        let found = segment_uses(&graph, &settings);

        assert_eq!(found.len(), 1);
        assert_eq!(uses(&found, 1), ["trigger", "condition", "goal"]);
    }

    #[test]
    fn a_schedule_audience_and_an_event_trigger_filter_are_filters() {
        let seg = id(2).to_string();
        let empty = graph(json!([]));
        let schedule = settings(json!({
            "trigger": {"type": "schedule", "cron": "0 9 * * 1", "timezone": "UTC",
                        "audience": {"type": "segment", "segment_id": seg}},
            "reentry": {"mode": "once"},
        }));
        let event = settings(json!({
            "trigger": {"type": "event", "event": "signed_up", "filters": [{"segment": seg}]},
            "reentry": {"mode": "once"},
        }));

        assert_eq!(uses(&segment_uses(&empty, &schedule), 2), ["filter"]);
        assert_eq!(uses(&segment_uses(&empty, &event), 2), ["filter"]);
    }

    #[test]
    fn several_segments_and_none_are_told_apart() {
        let settings = settings(json!({
            "trigger": {"type": "segment", "segment_id": id(3), "on": "exited"},
            "reentry": {"mode": "once"},
        }));
        let graph = graph(json!([
            {"id": "b", "type": "branch", "conditions": [
                {"any": [{"segment": id(4)}, {"list": id(5)}]}, {"trait": "x", "op": "exists"}]},
            {"id": "e", "type": "exit"},
        ]));

        let found = segment_uses(&graph, &settings);

        assert_eq!(uses(&found, 3), ["trigger"]);
        assert_eq!(uses(&found, 4), ["condition"]);
        assert_eq!(found.len(), 2);
        let manual = self::settings(
            json!({"trigger": {"type": "manual"}, "reentry": {"mode": "once"}}),
        );
        assert!(segment_uses(&self::graph(json!([])), &manual).is_empty());
    }
}
