//! Where each trait is stored, so promoting a trait to a column changes no call site.

/// How a trait is stored. Every trait is `jsonb` in `people.traits` until slot columns exist.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Storage {
    Jsonb,
}

/// A per-trait storage map. A trait it does not list is stored as `jsonb`.
#[derive(Clone, Debug, Default)]
pub struct TraitStorage {}

impl TraitStorage {
    pub fn of(&self, _key: &str) -> Storage {
        Storage::Jsonb
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_trait_is_jsonb_in_v1() {
        assert_eq!(TraitStorage::default().of("country"), Storage::Jsonb);
    }
}
