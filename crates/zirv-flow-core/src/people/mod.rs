//! The pure rules of the profile store (architecture sections 6 and 7): identifier kinds and
//! normalization, trait types and value validation, inference, and trait precedence.

mod identifier;
mod precedence;
mod rules;
mod traits;

pub use identifier::{Identifier, IdentifierError, IdentifierKind};
pub use precedence::{Contribution, Precedence, resolve};
pub use rules::{BlockReason, KindRule, merge_allowed, rule_for};
pub use traits::{DropReason, TraitType, TraitValue, coerce, infer_type, validate_key};
