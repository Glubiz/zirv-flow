//! The people endpoints: the field registry (architecture section 7.1) and identity conflict
//! review (section 7.3).

pub(crate) mod conflicts;
pub(crate) mod registry;

pub(crate) use registry::{
    CreateTraitRequest, TraitBody, UpdateTraitRequest, create, list, update,
};
