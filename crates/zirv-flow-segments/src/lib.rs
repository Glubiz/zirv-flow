//! Validation, normalization and SQL lowering of segment definitions (architecture sections 5.1
//! and 8).
//!
//! This crate is pure: it reads a [`Catalog`] snapshot the caller builds, emits SQL text and
//! parameters, and never touches a database. The reference evaluator lives here too, and behind
//! `test-support` the proptest generators that check the SQL against it.

mod bounds;
mod catalog;
mod compile;
mod deps;
mod evaluate;
#[cfg(any(test, feature = "test-support"))]
pub mod generate;
mod leaf;
mod lower;
mod normalize;
mod operand;
mod query;
mod snapshot;
mod storage;
mod stored_time;
mod time_value;
mod validate;

pub use bounds::event_lower_bounds;
pub use catalog::Catalog;
pub use compile::{CompiledSegment, compile};
pub use deps::Deps;
pub use evaluate::{EvalNode, Evaluation, Observed, Outcome, evaluate};
pub use normalize::normalize;
pub use query::{Param, Query};
pub use snapshot::{PersonSnapshot, SnapshotConsent, SnapshotEvent};
pub use storage::{Storage, TraitStorage};
pub use validate::{parse_and_validate, validate};
pub use zirv_flow_core::segment::{
    ChangeClasses, Definition, Issue, MAX_DEPTH, MAX_ISSUES, MAX_NODES, Node, code,
};
