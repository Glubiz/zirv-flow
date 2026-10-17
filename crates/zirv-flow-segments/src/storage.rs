//! Where each trait is stored, so promoting a trait to a column changes no call site.

use hashbrown::HashMap;
pub use zirv_flow_core::people::SlotColumn;

/// How a trait is stored. A trait is `jsonb` in `people.traits` unless a slot column holds it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Storage {
    Jsonb,
    /// A typed column on `people`, kept in step with the `jsonb` value by every writer.
    Slot(SlotColumn),
}

/// A per-trait storage map. A trait it does not list is stored as `jsonb`. The caller lists only
/// slots in state `ready`: a slot that is still filling or being retired is read from `jsonb`.
#[derive(Clone, Debug, Default)]
pub struct TraitStorage {
    slots: HashMap<String, SlotColumn>,
}

impl TraitStorage {
    /// A map that reads each listed trait from its slot.
    pub fn with_slots(slots: impl IntoIterator<Item = (String, SlotColumn)>) -> Self {
        Self {
            slots: slots.into_iter().collect(),
        }
    }

    pub fn of(&self, key: &str) -> Storage {
        self.slots
            .get(key)
            .map_or(Storage::Jsonb, |slot| Storage::Slot(*slot))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trait_is_jsonb_unless_listed() {
        assert_eq!(TraitStorage::default().of("country"), Storage::Jsonb);
        let storage = TraitStorage::with_slots([("plan".to_owned(), SlotColumn::S1)]);
        assert_eq!(storage.of("plan"), Storage::Slot(SlotColumn::S1));
        assert_eq!(storage.of("country"), Storage::Jsonb);
    }
}
