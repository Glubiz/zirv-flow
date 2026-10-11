//! Consent, the topic catalog and the consent side of a merge (architecture sections 6,
//! 7.3 and 10.1). Suppressions live in their own module.

mod merge;
mod state;
mod topics;

pub(crate) use merge::move_on_merge;
pub use state::{
    ConsentChange, ConsentEntry, ConsentHistoryEntry, ConsentOutcome, consent_for, consent_history,
    set_consent,
};
pub use topics::{
    NewTopic, Topic, TopicUpdate, create_topic, find_topic, find_topic_by_key, list_topics,
    update_topic,
};
