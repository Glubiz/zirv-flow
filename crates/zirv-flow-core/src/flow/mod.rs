//! The flow graph vocabulary and its structural publish validation (architecture sections 5.1
//! and 9.1). Pure: the checks that need the catalog or the database belong to the publish API.

mod graph;
mod issue;
mod settings;
mod validate;
mod webhook;

pub use graph::{
    Arm, Edge, EventMatch, FlowGraph, FlowNode, GRAPH_VERSION, ListChange, NODE_TYPES, NodeKind,
    PORT_ELSE, PORT_EVENT, PORT_NEXT, PORT_TIMEOUT, SyncOperation, TraitChange, UntilTime, Weekday,
    arm_port, case_port,
};
pub use issue::{Issue, code};
pub use settings::{
    Audience, ExitCriterion, FlowSettings, Goal, Reentry, SegmentChange, TRIGGER_TYPES, Trigger,
};
pub use validate::{
    MAX_ARMS, MAX_CONDITIONS, MAX_DURATION_SECONDS, MAX_EXIT_CRITERIA, MAX_ID_CHARS, MAX_ISSUES,
    MAX_NODES, MAX_SEND_DIMENSIONS, MAX_SYNC_TRAIT_KEYS, MAX_TRAIT_KEYS, MIN_ARMS, valid_id,
    validate, validate_graph, validate_settings,
};
pub use webhook::{
    Envelope, IDEMPOTENCY_HEADER, SIGNATURE_HEADER, SIGNING_GRACE_SECONDS, TIMESTAMP_HEADER,
    signature_value,
};
