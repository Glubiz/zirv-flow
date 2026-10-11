//! Save-time checks of a mapping against the file's header and the trait registry.

use std::collections::{HashMap, HashSet};

use super::apply::Header;
use super::model::{Mapping, Target, Transform};
use super::transform::valid_format;
use crate::people::{IdentifierKind, TraitType, validate_key};

/// The most column rules a mapping holds.
pub const MAX_RULES: usize = 200;
/// The most transforms on one column.
pub const MAX_TRANSFORMS: usize = 10;
/// The most columns a `concat` joins.
pub const MAX_CONCAT_COLUMNS: usize = 10;
/// The most entries a `value_map` holds.
pub const MAX_MAP_ENTRIES: usize = 1000;
/// The longest separator, constant or map text, in characters.
pub const MAX_TEXT_CHARS: usize = 256;

/// What the registry says about a trait key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TraitState {
    Missing,
    Archived,
    Active(TraitType),
}

/// The trait registry, as the mapping needs to see it.
pub trait TraitCatalog {
    fn trait_state(&self, key: &str) -> TraitState;
}

impl TraitCatalog for HashMap<String, TraitState> {
    fn trait_state(&self, key: &str) -> TraitState {
        self.get(key).copied().unwrap_or(TraitState::Missing)
    }
}

/// What is wrong with a mapping. A code and the column it concerns, never a value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MappingError {
    /// No column maps to an identifier.
    NoIdentifier,
    TooManyRules,
    /// A rule, the key column or a `concat` names a column the file does not have.
    UnknownColumn(String),
    InvalidIdentifierKind(String),
    InvalidTraitKey(String),
    /// The trait key is not in the registry.
    UnknownTrait(String),
    ArchivedTrait(String),
    /// Two columns map to the same trait.
    DuplicateTrait(String),
    /// A transform is malformed: a bad format, an empty separator, text over its cap.
    InvalidTransform(String),
}

impl MappingError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoIdentifier => "no_identifier",
            Self::TooManyRules => "too_many_rules",
            Self::UnknownColumn(_) => "unknown_column",
            Self::InvalidIdentifierKind(_) => "invalid_identifier_kind",
            Self::InvalidTraitKey(_) => "invalid_trait_key",
            Self::UnknownTrait(_) => "unknown_trait",
            Self::ArchivedTrait(_) => "archived_trait",
            Self::DuplicateTrait(_) => "duplicate_trait",
            Self::InvalidTransform(_) => "invalid_transform",
        }
    }

    /// The column the problem is on, when it is on one.
    pub fn column(&self) -> Option<&str> {
        match self {
            Self::NoIdentifier | Self::TooManyRules => None,
            Self::UnknownColumn(column)
            | Self::InvalidIdentifierKind(column)
            | Self::InvalidTraitKey(column)
            | Self::UnknownTrait(column)
            | Self::ArchivedTrait(column)
            | Self::DuplicateTrait(column)
            | Self::InvalidTransform(column) => Some(column),
        }
    }
}

/// Checks `mapping` against the file's `header` and the trait registry: at least one
/// identifier target, existing columns, valid identifier kinds, trait keys that exist and are
/// not archived, and well-formed transforms. Returns the first problem found.
pub fn validate(
    mapping: &Mapping,
    header: &Header,
    catalog: &dyn TraitCatalog,
) -> Result<(), MappingError> {
    if mapping.columns.len() > MAX_RULES {
        return Err(MappingError::TooManyRules);
    }
    if let Some(key) = &mapping.key_column
        && !header.contains(key)
    {
        return Err(MappingError::UnknownColumn(key.clone()));
    }
    let mut identifiers = 0;
    let mut traits = HashSet::new();
    for rule in &mapping.columns {
        if !header.contains(&rule.column) {
            return Err(MappingError::UnknownColumn(rule.column.clone()));
        }
        match &rule.target {
            Target::Identifier { kind } => {
                IdentifierKind::parse(kind)
                    .map_err(|_| MappingError::InvalidIdentifierKind(rule.column.clone()))?;
                identifiers += 1;
            }
            Target::Trait { key } => {
                validate_key(key)
                    .map_err(|_| MappingError::InvalidTraitKey(rule.column.clone()))?;
                match catalog.trait_state(key) {
                    TraitState::Missing => {
                        return Err(MappingError::UnknownTrait(rule.column.clone()));
                    }
                    TraitState::Archived => {
                        return Err(MappingError::ArchivedTrait(rule.column.clone()));
                    }
                    TraitState::Active(_) => {}
                }
                if !traits.insert(key.as_str()) {
                    return Err(MappingError::DuplicateTrait(rule.column.clone()));
                }
            }
            Target::List { .. } | Target::Ignore => {}
        }
        if rule.transforms.len() > MAX_TRANSFORMS {
            return Err(MappingError::InvalidTransform(rule.column.clone()));
        }
        for transform in &rule.transforms {
            check_transform(transform, header, &rule.column)?;
        }
    }
    if identifiers == 0 {
        return Err(MappingError::NoIdentifier);
    }
    Ok(())
}

fn short(text: &str) -> bool {
    text.chars().count() <= MAX_TEXT_CHARS
}

fn check_transform(
    transform: &Transform,
    header: &Header,
    column: &str,
) -> Result<(), MappingError> {
    let invalid = || MappingError::InvalidTransform(column.to_owned());
    match transform {
        Transform::Trim | Transform::Lower | Transform::Upper | Transform::Cast { .. } => Ok(()),
        Transform::Timestamp { format } => valid_format(format).then_some(()).ok_or_else(invalid),
        Transform::Concat { columns, separator } => {
            if columns.is_empty() || columns.len() > MAX_CONCAT_COLUMNS || !short(separator) {
                return Err(invalid());
            }
            match columns.iter().find(|other| !header.contains(other)) {
                Some(other) => Err(MappingError::UnknownColumn(other.clone())),
                None => Ok(()),
            }
        }
        Transform::Constant { value } => short(value).then_some(()).ok_or_else(invalid),
        Transform::ValueMap { map } => {
            let fits = map.len() <= MAX_MAP_ENTRIES
                && map.iter().all(|(from, to)| short(from) && short(to));
            fits.then_some(()).ok_or_else(invalid)
        }
        Transform::Split { separator } => (!separator.is_empty() && short(separator))
            .then_some(())
            .ok_or_else(invalid),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use uuid::Uuid;

    use super::super::model::ColumnRule;
    use super::*;

    fn header() -> Header {
        Header::new(
            ["Email", "Plan", "Name", "Last", "L"]
                .map(String::from)
                .to_vec(),
        )
        .unwrap()
    }

    fn catalog() -> HashMap<String, TraitState> {
        [
            ("plan".to_owned(), TraitState::Active(TraitType::String)),
            ("name".to_owned(), TraitState::Active(TraitType::String)),
            ("old".to_owned(), TraitState::Archived),
        ]
        .into_iter()
        .collect()
    }

    fn rule(column: &str, target: Target) -> ColumnRule {
        ColumnRule {
            column: column.into(),
            target,
            transforms: vec![],
        }
    }

    fn email() -> ColumnRule {
        rule(
            "Email",
            Target::Identifier {
                kind: "email".into(),
            },
        )
    }

    fn check(columns: Vec<ColumnRule>) -> Result<(), MappingError> {
        validate(
            &Mapping {
                key_column: None,
                columns,
            },
            &header(),
            &catalog(),
        )
    }

    #[test]
    fn a_mapping_with_an_identifier_and_known_traits_is_valid() {
        assert_eq!(
            check(vec![
                email(),
                rule("Plan", Target::Trait { key: "plan".into() }),
                rule(
                    "L",
                    Target::List {
                        list_id: Uuid::now_v7()
                    }
                ),
                rule("Name", Target::Ignore),
            ]),
            Ok(())
        );
    }

    #[test]
    fn a_mapping_needs_an_identifier_target() {
        assert_eq!(
            check(vec![rule("Plan", Target::Trait { key: "plan".into() })]),
            Err(MappingError::NoIdentifier)
        );
        assert_eq!(check(vec![]), Err(MappingError::NoIdentifier));
    }

    #[test]
    fn columns_must_exist_in_the_header() {
        assert_eq!(
            check(vec![
                email(),
                rule("Ghost", Target::Trait { key: "plan".into() })
            ]),
            Err(MappingError::UnknownColumn("Ghost".into()))
        );
        let mapping = Mapping {
            key_column: Some("Ghost".into()),
            columns: vec![email()],
        };
        assert_eq!(
            validate(&mapping, &header(), &catalog()),
            Err(MappingError::UnknownColumn("Ghost".into()))
        );
    }

    #[test]
    fn identifier_kinds_must_parse() {
        let bad = rule("Email", Target::Identifier { kind: "fax".into() });

        assert_eq!(
            check(vec![bad]),
            Err(MappingError::InvalidIdentifierKind("Email".into()))
        );
        let external = rule(
            "Email",
            Target::Identifier {
                kind: "external:crm".into(),
            },
        );
        assert_eq!(check(vec![external]), Ok(()));
    }

    #[test]
    fn trait_keys_must_exist_and_not_be_archived_or_repeated() {
        let with = |key: &str| {
            check(vec![
                email(),
                rule("Plan", Target::Trait { key: key.into() }),
            ])
        };

        assert_eq!(with("nope"), Err(MappingError::UnknownTrait("Plan".into())));
        assert_eq!(with("old"), Err(MappingError::ArchivedTrait("Plan".into())));
        assert_eq!(
            with("Bad Key"),
            Err(MappingError::InvalidTraitKey("Plan".into()))
        );
        assert_eq!(
            check(vec![
                email(),
                rule("Plan", Target::Trait { key: "plan".into() }),
                rule("Name", Target::Trait { key: "plan".into() }),
            ]),
            Err(MappingError::DuplicateTrait("Name".into()))
        );
    }

    #[test]
    fn transforms_are_checked() {
        let with = |transform| {
            let mut plan = rule("Plan", Target::Trait { key: "plan".into() });
            plan.transforms = vec![transform];
            check(vec![email(), plan])
        };
        let invalid = Err(MappingError::InvalidTransform("Plan".into()));

        assert_eq!(
            with(Transform::Timestamp {
                format: "%Y-%m-%d".into()
            }),
            Ok(())
        );
        assert_eq!(
            with(Transform::Timestamp {
                format: "%Q".into()
            }),
            invalid
        );
        assert_eq!(
            with(Transform::Split {
                separator: String::new()
            }),
            invalid
        );
        assert_eq!(
            with(Transform::Constant {
                value: "x".repeat(MAX_TEXT_CHARS + 1)
            }),
            invalid
        );
        assert_eq!(
            with(Transform::Concat {
                columns: vec![],
                separator: " ".into()
            }),
            invalid
        );
        assert_eq!(
            with(Transform::Concat {
                columns: vec!["Ghost".into()],
                separator: " ".into()
            }),
            Err(MappingError::UnknownColumn("Ghost".into()))
        );
        assert_eq!(
            with(Transform::Concat {
                columns: vec!["Last".into()],
                separator: " ".into()
            }),
            Ok(())
        );
        assert_eq!(
            with(Transform::ValueMap {
                map: BTreeMap::from([("a".to_owned(), "b".to_owned())])
            }),
            Ok(())
        );
    }

    #[test]
    fn a_mapping_is_bounded() {
        let many = (0..=MAX_RULES).map(|_| email()).collect();
        assert_eq!(check(many), Err(MappingError::TooManyRules));
        let mut crowded = email();
        crowded.transforms = vec![Transform::Trim; MAX_TRANSFORMS + 1];
        assert_eq!(
            check(vec![crowded]),
            Err(MappingError::InvalidTransform("Email".into()))
        );
    }

    #[test]
    fn errors_carry_a_code_and_a_column() {
        let error = MappingError::UnknownTrait("Plan".into());

        assert_eq!(error.code(), "unknown_trait");
        assert_eq!(error.column(), Some("Plan"));
        assert_eq!(MappingError::NoIdentifier.column(), None);
    }

    #[test]
    fn a_mapping_round_trips_through_json() {
        let json = serde_json::json!({
            "key_column": "Email",
            "columns": [
                {"column": "Email", "target": {"type": "identifier", "kind": "email"},
                 "transforms": [{"op": "trim"}, {"op": "lower"}]},
                {"column": "Plan", "target": {"type": "trait", "key": "plan"},
                 "transforms": [{"op": "cast", "to": "string"},
                                {"op": "value_map", "map": {"a": "b"}}]},
            ]
        });

        let mapping: Mapping = serde_json::from_value(json.clone()).unwrap();

        assert_eq!(serde_json::to_value(&mapping).unwrap(), json);
        assert!(
            serde_json::from_value::<Mapping>(serde_json::json!({"columns": [], "x": 1})).is_err()
        );
    }
}
