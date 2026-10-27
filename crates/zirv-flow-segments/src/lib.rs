//! Validation, normalization and SQL lowering of segment definitions (architecture sections 5.1
//! and 8).
//!
//! This crate is pure: it reads a [`Catalog`] snapshot the caller builds, emits SQL text and
//! parameters, and never touches a database. The reference evaluator lives here too, and behind
//! `test-support` the proptest generators that check the SQL against it.

mod bounds;
mod catalog;
mod compile;
mod condition;
mod deps;
mod evaluate;
#[cfg(any(test, feature = "test-support"))]
mod fuzz;
#[cfg(any(test, feature = "test-support"))]
pub mod generate;
mod leaf;
mod lower;
mod normalize;
mod operand;
mod policy;
mod query;
mod snapshot;
mod storage;
mod stored_time;
mod time_value;
mod validate;

pub use bounds::event_lower_bounds;
pub use catalog::Catalog;
pub use compile::{CompiledSegment, compile, compile_policy};
pub use condition::{ConditionError, compile_condition, parse_condition};
pub use deps::Deps;
pub use evaluate::{EvalNode, Evaluation, Observed, Outcome, evaluate, evaluate_policy};
#[cfg(any(test, feature = "test-support"))]
pub use fuzz::check_segment_ast;
pub use normalize::normalize;
pub use policy::{
    CompiledRule, PolicyFacts, RuleKind, SendContext, SnapshotMessage, compile_rules,
};
pub use query::{Param, Query};
pub use snapshot::{PersonSnapshot, SnapshotConsent, SnapshotEvent};
pub use storage::{SlotColumn, Storage, TraitStorage};
pub use time_value::{DurationError, format_duration, parse_duration};
pub use validate::{Context, parse_and_validate, parse_and_validate_in, validate, validate_in};
pub use zirv_flow_core::segment::{
    ChangeClasses, Definition, Issue, MAX_DEPTH, MAX_ISSUES, MAX_NODES, Node, code,
};
