//! The workspace facts a definition is checked against.

use hashbrown::{HashMap, HashSet};
use uuid::Uuid;
use zirv_flow_core::consent::Channel;
use zirv_flow_core::events::PropertyType;
use zirv_flow_core::people::TraitType;

/// A snapshot of what a workspace defines. The caller builds it; archived traits are left out.
/// External identifier namespaces are free-form and checked by the identifier parser alone.
#[derive(Clone, Debug, Default)]
pub struct Catalog {
    pub traits: HashMap<String, TraitType>,
    /// Each event name with the properties of its current schema version.
    pub events: HashMap<String, HashMap<String, PropertyType>>,
    pub lists: HashSet<Uuid>,
    pub segments: HashSet<Uuid>,
    /// The consent topics of each channel: each key with its id.
    pub topics: HashMap<Channel, HashMap<String, Uuid>>,
    /// The live consent dimensions: each key with its live value keys.
    pub dimensions: HashMap<String, HashSet<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_default_catalog_is_empty() {
        let catalog = Catalog::default();
        assert!(catalog.traits.is_empty());
        assert!(catalog.events.is_empty());
        assert!(catalog.lists.is_empty());
        assert!(catalog.segments.is_empty());
        assert!(catalog.topics.is_empty());
        assert!(catalog.dimensions.is_empty());
    }
}
