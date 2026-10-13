//! Validation and normalization of segment definitions (architecture sections 5.1 and 8).
//!
//! This crate is pure: it reads a [`Catalog`] snapshot the caller builds and never touches a
//! database. Lowering to SQL and the reference evaluator come later.

mod catalog;
mod normalize;
mod time_value;
mod validate;

pub use catalog::Catalog;
pub use normalize::normalize;
pub use validate::{parse_and_validate, validate};
pub use zirv_flow_core::segment::{
    Definition, Issue, MAX_DEPTH, MAX_ISSUES, MAX_NODES, Node, code,
};
