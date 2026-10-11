//! The profile store (architecture sections 6 and 7): the field registry, people and their
//! identities, the profile path of apply-batch, identity merges and conflict review.

mod apply;
mod conflicts;
mod merge;
mod profile;
mod profiles;
mod recompute;
mod registry;
mod resolve;
mod search;
mod timeline;

#[cfg(test)]
pub(crate) mod erasure;
#[cfg(test)]
mod isolation;

pub use apply::{
    DroppedTrait, ProfileOutcome, ProfileRecord, apply_profile_batch, remove_contribution,
};
pub(crate) use apply::{apply_profile_record, apply_profile_records};
pub(crate) use apply::{mark_changed, normalize_identifiers, record_resolution};
pub use conflicts::{
    ConflictResolution, ResolveError, find_conflict, list_conflicts, resolve_conflict,
};
pub use merge::{REWRITE_BATCH, REWRITE_JOB_KIND, current_person, rewrite_merged_person};
pub use profile::{IdentityGroup, Profile, ProfileMerge, ProfileTrait, load_profile};
pub use profiles::{
    IdentityConflict, Person, StoredIdentity, find_people_by_identifiers, find_person,
    find_person_by_identifier, list_identities, list_identity_conflicts,
};
pub use recompute::{RECOMPUTE_BATCH, recompute_trait_batch};
pub use registry::{
    NewTraitDefinition, TraitDefinition, TraitDefinitionUpdate, create_trait_definition,
    find_trait_definition, find_trait_definition_for_update, list_trait_definitions,
    update_trait_definition,
};
pub(crate) use resolve::{find_known_person, resolve_person};
pub use search::{PersonSearch, SearchError, SearchHit, SearchPage, TraitOp, search_people};
pub use timeline::{
    TimelineCursor, TimelineDecision, TimelineEntry, TimelineEvent, TimelineFilter, TimelineKind,
    TimelinePage, load_timeline,
};
