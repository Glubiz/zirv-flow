//! The kinds of change a segment depends on, shared by the compiler and the store (ruling
//! 6645b860). A change to a person marks the segments whose classes include its kind.

use std::ops::BitOr;

/// A set of change classes, kept as a bitmask.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct ChangeClasses(u8);

impl ChangeClasses {
    pub const NONE: Self = Self(0);
    pub const TRAITS: Self = Self(1);
    pub const EVENTS: Self = Self(2);
    pub const LISTS: Self = Self(4);
    pub const CONSENT: Self = Self(8);
    pub const IDENTIFIERS: Self = Self(16);
    pub const MERGE: Self = Self(32);

    pub const fn bits(self) -> u8 {
        self.0
    }

    /// The set for a stored bitmask. Bits that name no class are dropped.
    pub const fn from_bits(bits: u8) -> Self {
        Self(bits & 63)
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }
}

impl BitOr for ChangeClasses {
    type Output = Self;

    fn bitor(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_classes_are_distinct_bits_with_fixed_values() {
        let all = [
            ChangeClasses::TRAITS,
            ChangeClasses::EVENTS,
            ChangeClasses::LISTS,
            ChangeClasses::CONSENT,
            ChangeClasses::IDENTIFIERS,
            ChangeClasses::MERGE,
        ];
        let bits: Vec<u8> = all.iter().map(|class| class.bits()).collect();
        assert_eq!(bits, [1, 2, 4, 8, 16, 32]);
    }

    #[test]
    fn union_contains_and_intersects() {
        let set = ChangeClasses::TRAITS | ChangeClasses::LISTS;
        assert_eq!(set.bits(), 5);
        assert!(set.contains(ChangeClasses::TRAITS));
        assert!(!set.contains(ChangeClasses::TRAITS | ChangeClasses::EVENTS));
        assert!(set.intersects(ChangeClasses::LISTS | ChangeClasses::EVENTS));
        assert!(!set.intersects(ChangeClasses::MERGE));
        assert!(!ChangeClasses::NONE.intersects(set));
        assert_eq!(ChangeClasses::default(), ChangeClasses::NONE);
    }

    #[test]
    fn from_bits_drops_unknown_bits() {
        assert_eq!(ChangeClasses::from_bits(255).bits(), 63);
        assert_eq!(ChangeClasses::from_bits(9).bits(), 9);
    }
}
