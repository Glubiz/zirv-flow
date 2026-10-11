//! The people endpoints: for now the field registry (architecture section 7.1).

pub(crate) mod registry;

pub(crate) use registry::{
    CreateTraitRequest, TraitBody, UpdateTraitRequest, create, list, update,
};
