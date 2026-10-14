//! What the reference evaluator reads about one person (architecture section 8.2). The caller
//! loads it; the evaluator never touches a database.

use std::collections::BTreeSet;

use serde_json::{Map, Value};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::consent::{Channel, ConsentStatus};

/// One person's data, as the compiled SQL would see it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PersonSnapshot {
    /// `people.traits`: stored values by key, whatever their JSON type.
    pub traits: Map<String, Value>,
    pub events: Vec<SnapshotEvent>,
    /// The event names whose events were truncated when the snapshot was loaded. A condition on
    /// one of them cannot be decided from the events held (ruling 0c6404db).
    pub capped_events: BTreeSet<String>,
    pub lists: BTreeSet<Uuid>,
    pub consent: Vec<SnapshotConsent>,
    /// The identifier kinds the person holds: `email`, `phone`, `anonymous_id` or
    /// `external:<namespace>`.
    pub identifier_kinds: BTreeSet<String>,
    /// The segments the person is an open member of.
    pub segments: BTreeSet<Uuid>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SnapshotEvent {
    pub name: String,
    pub occurred_at: OffsetDateTime,
    pub properties: Map<String, Value>,
}

/// One row of `consent`: a channel or a topic, and what the person said.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnapshotConsent {
    pub channel: Channel,
    /// `None` is the channel itself.
    pub topic: Option<Uuid>,
    pub status: ConsentStatus,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_default_snapshot_holds_nothing() {
        let person = PersonSnapshot::default();
        assert!(person.traits.is_empty());
        assert!(person.events.is_empty());
        assert!(person.capped_events.is_empty());
        assert!(person.lists.is_empty());
        assert!(person.consent.is_empty());
        assert!(person.identifier_kinds.is_empty());
        assert!(person.segments.is_empty());
    }
}
