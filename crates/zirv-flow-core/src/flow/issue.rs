//! What is wrong with a flow: a fixed code and the node or edge it concerns, never a value.

use std::fmt;

use serde::Serialize;

/// The fixed issue codes.
pub mod code {
    // The graph document.
    pub const UNSUPPORTED_VERSION: &str = "unsupported_version";
    pub const TOO_MANY_NODES: &str = "too_many_nodes";
    pub const INVALID_ID: &str = "invalid_id";
    pub const DUPLICATE_NODE_ID: &str = "duplicate_node_id";
    pub const DUPLICATE_EDGE_ID: &str = "duplicate_edge_id";
    // Edges and ports.
    pub const UNKNOWN_NODE: &str = "unknown_node";
    pub const UNKNOWN_PORT: &str = "unknown_port";
    pub const PORT_WIRED_TWICE: &str = "port_wired_twice";
    pub const PORT_NOT_WIRED: &str = "port_not_wired";
    // Shape.
    pub const NO_ENTRY: &str = "no_entry";
    pub const MULTIPLE_ENTRIES: &str = "multiple_entries";
    pub const CYCLE: &str = "cycle";
    pub const UNREACHABLE: &str = "unreachable";
    // Node fields.
    pub const INVALID_DURATION: &str = "invalid_duration";
    pub const INVALID_TIME: &str = "invalid_time";
    pub const INVALID_WEEKDAYS: &str = "invalid_weekdays";
    pub const INVALID_EVENT_NAME: &str = "invalid_event_name";
    pub const INVALID_FILTER: &str = "invalid_filter";
    pub const NO_CONDITIONS: &str = "no_conditions";
    pub const TOO_MANY_CONDITIONS: &str = "too_many_conditions";
    pub const INVALID_CONDITION: &str = "invalid_condition";
    pub const TOO_FEW_ARMS: &str = "too_few_arms";
    pub const TOO_MANY_ARMS: &str = "too_many_arms";
    pub const INVALID_WEIGHT: &str = "invalid_weight";
    pub const WEIGHTS_NOT_100: &str = "weights_not_100";
    pub const INVALID_URL: &str = "invalid_url";
    pub const INVALID_TRAIT_KEY: &str = "invalid_trait_key";
    pub const TOO_MANY_TRAIT_KEYS: &str = "too_many_trait_keys";
    // Settings.
    pub const INVALID_CRON: &str = "invalid_cron";
    pub const INVALID_TIMEZONE: &str = "invalid_timezone";
    pub const EXIT_ON_GOAL_WITHOUT_GOAL: &str = "exit_on_goal_without_goal";
    pub const GOAL_CRITERION_WITHOUT_GOAL: &str = "goal_criterion_without_goal";
    pub const LEAVES_SEGMENT_WITHOUT_SEGMENT_TRIGGER: &str =
        "leaves_segment_without_segment_trigger";

    /// Every code.
    pub const ALL: [&str; 33] = [
        UNSUPPORTED_VERSION,
        TOO_MANY_NODES,
        INVALID_ID,
        DUPLICATE_NODE_ID,
        DUPLICATE_EDGE_ID,
        UNKNOWN_NODE,
        UNKNOWN_PORT,
        PORT_WIRED_TWICE,
        PORT_NOT_WIRED,
        NO_ENTRY,
        MULTIPLE_ENTRIES,
        CYCLE,
        UNREACHABLE,
        INVALID_DURATION,
        INVALID_TIME,
        INVALID_WEEKDAYS,
        INVALID_EVENT_NAME,
        INVALID_FILTER,
        NO_CONDITIONS,
        TOO_MANY_CONDITIONS,
        INVALID_CONDITION,
        TOO_FEW_ARMS,
        TOO_MANY_ARMS,
        INVALID_WEIGHT,
        WEIGHTS_NOT_100,
        INVALID_URL,
        INVALID_TRAIT_KEY,
        TOO_MANY_TRAIT_KEYS,
        INVALID_CRON,
        INVALID_TIMEZONE,
        EXIT_ON_GOAL_WITHOUT_GOAL,
        GOAL_CRITERION_WITHOUT_GOAL,
        LEAVES_SEGMENT_WITHOUT_SEGMENT_TRIGGER,
    ];
}

/// One problem: a fixed code, the node or edge it concerns, and where inside it.
///
/// `at` is a JSON pointer into the node, edge or settings object (for example
/// `/conditions/1/trait`, or `/ports/else` for an unwired port). Segment issues found inside a
/// condition keep their own pointer under the condition's. It never holds a value from the flow.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Issue {
    pub code: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edge: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
}

impl Issue {
    /// An issue about the graph or the settings as a whole.
    pub fn general(code: &'static str) -> Self {
        Self {
            code,
            node: None,
            edge: None,
            at: None,
        }
    }

    pub fn node(code: &'static str, node: &str) -> Self {
        Self {
            node: Some(node.to_owned()),
            ..Self::general(code)
        }
    }

    pub fn edge(code: &'static str, edge: &str) -> Self {
        Self {
            edge: Some(edge.to_owned()),
            ..Self::general(code)
        }
    }

    /// The same issue, located inside its node, edge or settings object.
    pub fn at(mut self, pointer: impl Into<String>) -> Self {
        self.at = Some(pointer.into());
        self
    }
}

impl fmt::Display for Issue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code)?;
        if let Some(node) = &self.node {
            write!(formatter, " node {node}")?;
        }
        if let Some(edge) = &self.edge {
            write!(formatter, " edge {edge}")?;
        }
        if let Some(at) = &self.at {
            write!(formatter, " at {at}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_codes_are_unique_snake_case() {
        let mut seen = std::collections::BTreeSet::new();
        for text in code::ALL {
            assert!(seen.insert(text), "{text} is listed twice");
            assert!(
                text.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            );
        }
    }

    #[test]
    fn an_issue_displays_and_serializes_only_what_it_has() {
        let issue = Issue::node(code::PORT_NOT_WIRED, "n1").at("/ports/else");
        assert_eq!(issue.to_string(), "port_not_wired node n1 at /ports/else");
        assert_eq!(
            serde_json::to_value(&issue).unwrap(),
            serde_json::json!({"code": "port_not_wired", "node": "n1", "at": "/ports/else"})
        );
        assert_eq!(Issue::edge(code::CYCLE, "e1").to_string(), "cycle edge e1");
    }
}
