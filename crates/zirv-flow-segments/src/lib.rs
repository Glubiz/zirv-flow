//! Validation, normalization and SQL lowering of segment definitions (architecture sections 5.1
//! and 8).
//!
//! This crate is pure: it reads a [`Catalog`] snapshot the caller builds, emits SQL text and
//! parameters, and never touches a database. The reference evaluator comes later.

mod catalog;
mod compile;
mod deps;
mod lower;
mod normalize;
mod query;
mod storage;
mod time_value;
mod validate;

pub use catalog::Catalog;
pub use compile::{CompiledSegment, compile};
pub use deps::Deps;
pub use normalize::normalize;
pub use query::{Param, Query};
pub use storage::{Storage, TraitStorage};
pub use validate::{parse_and_validate, validate};
pub use zirv_flow_core::segment::{
    ChangeClasses, Definition, Issue, MAX_DEPTH, MAX_ISSUES, MAX_NODES, Node, code,
};
