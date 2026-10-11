//! The identity rules (architecture section 7.3): how many identifiers of one kind a person
//! may hold, and whether two people may be merged.

use std::collections::{BTreeMap, BTreeSet};

use super::identifier::{Identifier, IdentifierKind};

/// What a person may hold of one identifier kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KindRule {
    /// A person holds at most one distinct value of the kind.
    pub unique_per_person: bool,
    /// The most distinct values of the kind one person holds.
    pub max_per_person: usize,
}

/// The rule of a kind. These defaults are not configurable yet.
pub const fn rule_for(kind: &IdentifierKind) -> KindRule {
    match kind {
        IdentifierKind::External(_) => KindRule {
            unique_per_person: true,
            max_per_person: 1,
        },
        IdentifierKind::Email | IdentifierKind::Phone => KindRule {
            unique_per_person: false,
            max_per_person: 10,
        },
        IdentifierKind::AnonymousId => KindRule {
            unique_per_person: false,
            max_per_person: 100,
        },
    }
}

/// Why a merge, or an identifier move, is not allowed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockReason {
    /// Two different values of a kind that a person holds once.
    UniqueKind,
    /// More values of a kind than a person may hold.
    MaxPerPerson,
}

impl BlockReason {
    /// A stable, static reason for conflict rows, decision records and API answers.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::UniqueKind => "unique_kind",
            Self::MaxPerPerson => "max_per_person",
        }
    }
}

/// Whether one person may hold all of `identities` (the union over the people to merge, plus
/// any new identifiers). The same identifier given twice counts once.
pub fn merge_allowed<'a>(
    identities: impl IntoIterator<Item = &'a Identifier>,
) -> Result<(), BlockReason> {
    let mut by_kind: BTreeMap<String, BTreeSet<&str>> = BTreeMap::new();
    let mut rules: BTreeMap<String, KindRule> = BTreeMap::new();
    for identity in identities {
        let name = identity.kind().as_text();
        rules
            .entry(name.clone())
            .or_insert_with(|| rule_for(identity.kind()));
        by_kind.entry(name).or_default().insert(identity.value());
    }
    let mut blocked = None;
    for (name, values) in &by_kind {
        let rule = rules[name];
        if rule.unique_per_person && values.len() > 1 {
            return Err(BlockReason::UniqueKind);
        }
        if values.len() > rule.max_per_person {
            blocked = Some(BlockReason::MaxPerPerson);
        }
    }
    blocked.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(kind: &str, value: &str) -> Identifier {
        Identifier::from_stored(IdentifierKind::parse(kind).unwrap(), value.to_owned())
    }

    fn many(kind: &str, count: usize) -> Vec<Identifier> {
        (0..count).map(|n| id(kind, &format!("v{n}"))).collect()
    }

    #[test]
    fn the_default_rules_per_kind() {
        let external = rule_for(&IdentifierKind::External("crm".into()));
        assert!(external.unique_per_person);
        assert_eq!(external.max_per_person, 1);
        assert_eq!(rule_for(&IdentifierKind::Email).max_per_person, 10);
        assert_eq!(rule_for(&IdentifierKind::Phone).max_per_person, 10);
        assert_eq!(rule_for(&IdentifierKind::AnonymousId).max_per_person, 100);
        assert!(!rule_for(&IdentifierKind::Email).unique_per_person);
    }

    #[test]
    fn two_values_of_a_unique_kind_block_the_merge() {
        let identities = [id("external:crm", "1"), id("external:crm", "2")];
        assert_eq!(merge_allowed(&identities), Err(BlockReason::UniqueKind));
    }

    #[test]
    fn the_same_unique_value_twice_and_different_namespaces_are_allowed() {
        let same = [id("external:crm", "1"), id("external:crm", "1")];
        let namespaces = [id("external:crm", "1"), id("external:shop", "9")];
        assert_eq!(merge_allowed(&same), Ok(()));
        assert_eq!(merge_allowed(&namespaces), Ok(()));
    }

    #[test]
    fn a_kind_over_its_maximum_blocks_and_at_the_maximum_passes() {
        assert_eq!(merge_allowed(&many("email", 10)), Ok(()));
        assert_eq!(
            merge_allowed(&many("email", 11)),
            Err(BlockReason::MaxPerPerson)
        );
        assert_eq!(merge_allowed(&many("anonymous_id", 100)), Ok(()));
        assert_eq!(
            merge_allowed(&many("anonymous_id", 101)),
            Err(BlockReason::MaxPerPerson)
        );
    }

    #[test]
    fn duplicates_do_not_count_toward_the_maximum() {
        let mut identities = many("phone", 10);
        identities.extend(many("phone", 10));
        assert_eq!(merge_allowed(&identities), Ok(()));
    }

    #[test]
    fn unique_kind_is_reported_before_the_maximum() {
        let mut identities = many("email", 11);
        identities.extend([id("external:crm", "1"), id("external:crm", "2")]);
        assert_eq!(merge_allowed(&identities), Err(BlockReason::UniqueKind));
    }

    #[test]
    fn an_empty_union_is_allowed_and_reasons_are_stable() {
        assert_eq!(merge_allowed(&[]), Ok(()));
        assert_eq!(BlockReason::UniqueKind.as_str(), "unique_kind");
        assert_eq!(BlockReason::MaxPerPerson.as_str(), "max_per_person");
    }
}
