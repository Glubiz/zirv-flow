//! Consent, the topic catalog and the consent side of a merge (architecture sections 6,
//! 7.3 and 10.1). Suppressions live in their own module.

mod dimensions;
mod merge;
mod state;
mod topics;

pub use dimensions::{
    Dimension, DimensionUpdate, DimensionValue, NewDimension, create_dimension,
    create_dimension_value, find_dimension, list_dimensions, unknown_dimension_entry,
    update_dimension, update_dimension_value,
};
pub(crate) use merge::move_on_merge;
pub use state::{
    ConsentChange, ConsentEntry, ConsentExportCursor, ConsentExportRow, ConsentHistoryEntry,
    ConsentOutcome, ReadOutcome, consent_export_page, consent_for, consent_history, consent_read,
    set_consent,
};
pub use topics::{
    NewTopic, Topic, TopicUpdate, create_topic, find_topic, find_topic_by_key, list_topics,
    update_topic,
};
