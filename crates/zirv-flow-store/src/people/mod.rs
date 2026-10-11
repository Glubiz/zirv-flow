//! The profile store (architecture sections 6 and 7): the field registry, people and their
//! identities, and the profile path of apply-batch.

mod apply;
mod profiles;
mod registry;
mod resolve;

#[cfg(test)]
mod erasure;
#[cfg(test)]
mod isolation;

pub use apply::{
    DroppedTrait, ProfileOutcome, ProfileRecord, apply_profile_batch, remove_contribution,
};
pub use profiles::{
    IdentityConflict, Person, StoredIdentity, find_person, find_person_by_identifier,
    list_identities, list_identity_conflicts,
};
pub use registry::{
    NewTraitDefinition, TraitDefinition, TraitDefinitionUpdate, create_trait_definition,
    find_trait_definition, list_trait_definitions, update_trait_definition,
};
