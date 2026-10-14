//! What a compiled segment reads, so the store can tell which changes affect it.

use std::collections::BTreeSet;

use serde_json::{Value, json};
use uuid::Uuid;
use zirv_flow_core::segment::ChangeClasses;

/// The inputs a segment reads. The sets are ordered, so the output is deterministic.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Deps {
    pub traits: BTreeSet<String>,
    pub events: BTreeSet<String>,
    pub lists: BTreeSet<Uuid>,
    pub segments: BTreeSet<Uuid>,
    /// Each consent channel with its topic key, `None` for the channel itself.
    pub consent: BTreeSet<(String, Option<String>)>,
    pub identifiers: BTreeSet<String>,
    pub classes: ChangeClasses,
}

impl Deps {
    /// The JSON form, for fixtures.
    pub fn to_value(&self) -> Value {
        json!({
            "traits": self.traits,
            "events": self.events,
            "lists": self.lists.iter().map(Uuid::to_string).collect::<Vec<_>>(),
            "segments": self.segments.iter().map(Uuid::to_string).collect::<Vec<_>>(),
            "consent": self.consent,
            "identifiers": self.identifiers,
            "classes": self.classes.bits(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deps_serialize_in_a_fixed_shape() {
        let mut deps = Deps::default();
        deps.traits.insert("age".into());
        deps.consent.insert(("email".into(), None));
        deps.classes = ChangeClasses::TRAITS | ChangeClasses::MERGE;
        assert_eq!(
            deps.to_value(),
            json!({
                "traits": ["age"],
                "events": [],
                "lists": [],
                "segments": [],
                "consent": [["email", null]],
                "identifiers": [],
                "classes": 33,
            })
        );
    }
}
