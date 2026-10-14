//! The segment definition AST, version 1 (architecture sections 5.1 and 8). Only the shape lives
//! here; validation and normalization are in `zirv-flow-segments`.

mod ast;
mod classes;
mod issue;
mod parse;

pub use ast::{
    AggFn, Aggregate, ConsentCondition, Definition, EventCondition, NODE_KEYS, Node, Op,
    PropertyFilter, TraitCondition, Window,
};
pub use classes::ChangeClasses;
pub use issue::{Issue, code, pointer_push};
pub use parse::{MAX_DEPTH, MAX_ISSUES, MAX_NODES, parse};
