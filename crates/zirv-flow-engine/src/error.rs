//! The typed errors of [`step`](crate::step): input the engine refuses, never a panic.

use std::error::Error;
use std::fmt;

/// Why a step could not run. None of these changes the enrollment; the caller reports them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EngineError {
    /// The state cannot take this input (a wrong status, a node of the wrong kind, a version
    /// that cannot grow).
    InvalidState(String),
    /// The input does not fit the state (another node, another event, a wrong result count).
    InputMismatch(String),
    /// A node id, from the state or from an edge, that the graph does not have.
    UnknownNode(String),
    /// A node whose output port has no edge.
    MissingEdge { node: String, port: String },
    /// A graph that kept advancing for more steps than it has nodes, so it has a cycle.
    Cycle,
    /// A graph with no entry node.
    NoEntry,
}

impl fmt::Display for EngineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidState(why) => write!(formatter, "invalid enrollment state: {why}"),
            Self::InputMismatch(why) => write!(formatter, "input does not fit the state: {why}"),
            Self::UnknownNode(node) => write!(formatter, "unknown node `{node}`"),
            Self::MissingEdge { node, port } => {
                write!(formatter, "node `{node}` has no edge on port `{port}`")
            }
            Self::Cycle => formatter.write_str("the graph has a cycle"),
            Self::NoEntry => formatter.write_str("the graph has no entry node"),
        }
    }
}

impl Error for EngineError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_error_has_a_message() {
        let errors = [
            EngineError::InvalidState("x".into()),
            EngineError::InputMismatch("x".into()),
            EngineError::UnknownNode("n".into()),
            EngineError::MissingEdge {
                node: "n".into(),
                port: "next".into(),
            },
            EngineError::Cycle,
            EngineError::NoEntry,
        ];
        for error in errors {
            assert!(!error.to_string().is_empty());
        }
    }
}
