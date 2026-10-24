//! Consent, the topic catalog and the consent side of a merge (architecture sections 6,
//! 7.3 and 10.1). Suppressions live in their own module.

mod dimensions;
mod merge;
mod policy;
mod state;
mod topics;

pub use dimensions::{
    Dimension, DimensionUpdate, DimensionValue, NewDimension, create_dimension,
    create_dimension_value, find_dimension, list_dimensions, unknown_dimension_entry,
    update_dimension, update_dimension_value,
};
pub(crate) use merge::move_on_merge;
pub use policy::{
    ConsentPolicy, DecidingRule, PolicyVersion, RuleCheck, RuleToCheck, consent_policy_draft,
    evaluate_consent_policy, find_consent_policy, list_consent_policy_versions,
    lock_consent_policy, publish_consent_policy, published_consent_policy,
    save_consent_policy_draft, save_consent_policy_draft_at,
};
pub use state::{
    ConsentChange, ConsentEntry, ConsentExportCursor, ConsentExportRow, ConsentHistoryEntry,
    ConsentOutcome, ReadOutcome, consent_export_page, consent_for, consent_history, consent_read,
    dimensions_of, set_consent,
};
pub use topics::{
    NewTopic, Topic, TopicUpdate, create_topic, find_topic, find_topic_by_key, list_topics,
    update_topic,
};
