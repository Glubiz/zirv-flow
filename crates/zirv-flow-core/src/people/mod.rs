//! The pure rules of the profile store (architecture sections 6 and 7): identifier kinds and
//! normalization, trait types and value validation, inference, and trait precedence.

mod identifier;
mod precedence;
mod rules;
mod slots;
mod traits;

pub use identifier::{Identifier, IdentifierError, IdentifierKind, identifier_hash};
pub use precedence::{Contribution, Precedence, resolve};
pub use rules::{BlockReason, KindRule, merge_allowed, rule_for};
pub use slots::{
    DATE_SHAPE, SlotColumn, SlotKind, SlotState, TIMESTAMP_SHAPE, UnknownSlot, boolean_sql,
    number_sql, string_sql, time_sql,
};
pub use traits::{DropReason, TraitType, TraitValue, coerce, infer_type, validate_key};
