//! The pure rules of native contacts (architecture section 7.6): the contact definition, the
//! contact key and the validation of a contact's fields against the definition.
//!
//! The validation reuses the event rules of [`crate::events`], so a contact and an event
//! property fail for the same reasons. Problems name fields and reason codes, never values.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::ValidationError;
use crate::events::{PropertyType, SchemaProperty, check_properties};
use crate::people::{Identifier, IdentifierKind, TraitType, validate_key};

/// The identifier kind of the contact key, which every contact has.
pub const KEY_KIND: &str = "external:contact_key";
/// The `source` of a contact's record in the ledger.
pub const SOURCE: &str = "native";
/// The `stream` of a contact's record in the ledger.
pub const STREAM: &str = "contacts";

const MAX_KEY_CHARS: usize = 256;
const MAX_FIELDS: usize = 100;

/// One field of the contact definition. The registry entry named by `key` supplies the type.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContactField {
    /// A key of the field registry.
    pub key: String,
    #[serde(default)]
    pub required: bool,
    /// `email`, `phone`, `anonymous_id` or `external:<name>`: the field's value also becomes
    /// an identity of the contact's person. Null for an ordinary field.
    #[serde(default)]
    pub identifier: Option<String>,
}

/// A definition field with the registry type of its key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TypedField {
    pub field: ContactField,
    pub trait_type: TraitType,
}

/// A key of the field registry as a contact write sees it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegistryKey {
    pub key: String,
    pub trait_type: TraitType,
    pub archived: bool,
}

/// The path segments of the contacts API that are not contact keys.
const RESERVED_KEYS: [&str; 2] = ["search", "bulk"];

/// A contact key: 1 to 256 characters without control characters, and not `search` or `bulk`,
/// which name endpoints.
pub fn validate_contact_key(key: &str) -> Result<(), ValidationError> {
    let length = key.chars().count();
    if (1..=MAX_KEY_CHARS).contains(&length)
        && !key.chars().any(char::is_control)
        && !RESERVED_KEYS.contains(&key)
    {
        Ok(())
    } else {
        Err(ValidationError::new(
            "key",
            "must be 1 to 256 characters without control characters, and not search or bulk",
        ))
    }
}

/// Checks the shape of a definition: at most 100 fields with valid, distinct keys, and
/// identifier kinds that are known, distinct and not the contact key's own. Whether each key
/// is in the registry is the caller's check.
pub fn validate_definition(fields: &[ContactField]) -> Result<(), ValidationError> {
    if fields.len() > MAX_FIELDS {
        return Err(ValidationError::new(
            "fields",
            "must list at most 100 fields",
        ));
    }
    let mut keys = HashSet::new();
    let mut kinds = HashSet::new();
    for field in fields {
        validate_key(&field.key)
            .map_err(|_| ValidationError::new("fields", "each key must be a valid field key"))?;
        if !keys.insert(field.key.as_str()) {
            return Err(ValidationError::new("fields", "each key must appear once"));
        }
        let Some(kind) = &field.identifier else {
            continue;
        };
        let parsed = IdentifierKind::parse(kind).map_err(|_| {
            ValidationError::new(
                "fields",
                "each identifier must be email, phone, anonymous_id or external:<name>",
            )
        })?;
        if parsed.as_text() == KEY_KIND {
            return Err(ValidationError::new(
                "fields",
                "the contact key is implicit and cannot be an identifier field",
            ));
        }
        if !kinds.insert(parsed.as_text()) {
            return Err(ValidationError::new(
                "fields",
                "each identifier kind may belong to one field",
            ));
        }
    }
    Ok(())
}

/// One invalid field and why: a stable code such as `missing_required`, `wrong_type`,
/// `unplanned_property`, `invalid_key` or `invalid_email`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FieldProblem {
    pub field: String,
    pub reason: &'static str,
}

/// A contact write, validated: what the profile path receives.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Planned {
    /// `(kind, value)` pairs, the contact key first.
    pub identifiers: Vec<(String, String)>,
    /// The fields to store as traits, identifier fields included, so a read returns what
    /// was written.
    pub traits: Map<String, Value>,
}

const fn property_type(trait_type: TraitType) -> PropertyType {
    match trait_type {
        TraitType::String => PropertyType::String,
        TraitType::Number => PropertyType::Number,
        TraitType::Boolean => PropertyType::Boolean,
        TraitType::Timestamp => PropertyType::Timestamp,
        TraitType::Date => PropertyType::Date,
        TraitType::StringList => PropertyType::StringList,
    }
}

/// Validates the complete `fields` of contact `key` (a JSON `null` counts as absent) against
/// `definition`, and maps them to identifiers and traits.
///
/// With a definition a contact may hold only its fields, and every required one. Without one
/// only the contact key is required, any valid registry key is accepted, and the profile
/// path checks each value against the registry.
pub fn plan(
    definition: Option<&[TypedField]>,
    key: &str,
    fields: &Map<String, Value>,
) -> Result<Planned, Vec<FieldProblem>> {
    let mut problems = Vec::new();
    let present: Map<String, Value> = fields
        .iter()
        .filter(|(_, value)| !value.is_null())
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    let mut identifiers = vec![(KEY_KIND.to_owned(), key.to_owned())];
    match definition {
        Some(definition) => {
            let schema: Vec<SchemaProperty> = definition
                .iter()
                .map(|typed| SchemaProperty {
                    name: typed.field.key.clone(),
                    property_type: property_type(typed.trait_type),
                    required: typed.field.required,
                    enum_values: None,
                    description: String::new(),
                    pii: false,
                })
                .collect();
            problems.extend(
                check_properties(&present, &schema)
                    .into_iter()
                    .map(|reason| FieldProblem {
                        field: reason.property,
                        reason: reason.reason.as_str(),
                    }),
            );
            for typed in definition {
                let (Some(kind), Some(Value::String(value))) =
                    (&typed.field.identifier, present.get(&typed.field.key))
                else {
                    continue;
                };
                match Identifier::parse(kind, value) {
                    Ok(_) => identifiers.push((kind.clone(), value.clone())),
                    Err(error) => problems.push(FieldProblem {
                        field: typed.field.key.clone(),
                        reason: error.reason(),
                    }),
                }
            }
        }
        None => problems.extend(
            present
                .keys()
                .filter(|name| validate_key(name).is_err())
                .map(|name| FieldProblem {
                    field: name.clone(),
                    reason: "invalid_key",
                }),
        ),
    }
    if problems.is_empty() {
        Ok(Planned {
            identifiers,
            traits: present,
        })
    } else {
        Err(problems)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn field(key: &str, required: bool, identifier: Option<&str>) -> ContactField {
        ContactField {
            key: key.to_owned(),
            required,
            identifier: identifier.map(str::to_owned),
        }
    }

    fn typed(key: &str, required: bool, identifier: Option<&str>, kind: TraitType) -> TypedField {
        TypedField {
            field: field(key, required, identifier),
            trait_type: kind,
        }
    }

    fn map(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap()
    }

    fn definition() -> Vec<TypedField> {
        vec![
            typed("email", true, Some("email"), TraitType::String),
            typed("age", false, None, TraitType::Number),
            typed("tags", false, None, TraitType::StringList),
        ]
    }

    fn problems(result: Result<Planned, Vec<FieldProblem>>) -> Vec<(String, &'static str)> {
        result
            .unwrap_err()
            .into_iter()
            .map(|problem| (problem.field, problem.reason))
            .collect()
    }

    #[test]
    fn a_contact_key_is_one_to_256_characters_without_controls() {
        assert!(validate_contact_key("a").is_ok());
        assert!(validate_contact_key(&"x".repeat(256)).is_ok());
        for bad in [String::new(), "x".repeat(257), "a\nb".to_owned()] {
            assert!(validate_contact_key(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_definition_of_registry_style_keys_and_distinct_identifiers_is_valid() {
        let fields = [
            field("email", true, Some("email")),
            field("crm_id", false, Some("external:crm")),
            field("plan", false, None),
        ];

        assert!(validate_definition(&fields).is_ok());
        assert!(validate_definition(&[]).is_ok());
    }

    #[test]
    fn a_malformed_definition_is_refused() {
        let cases = [
            vec![field("Bad Key", false, None)],
            vec![field("a", false, None), field("a", true, None)],
            vec![field("a", false, Some("fax"))],
            vec![field("a", false, Some("external:contact_key"))],
            vec![
                field("a", false, Some("email")),
                field("b", false, Some("email")),
            ],
            (0..101)
                .map(|n| field(&format!("f{n}"), false, None))
                .collect(),
        ];
        for fields in cases {
            let error = validate_definition(&fields).unwrap_err();
            assert_eq!(error.field(), "fields");
        }
    }

    #[test]
    fn a_valid_contact_maps_to_identifiers_and_traits() {
        let planned = plan(
            Some(&definition()),
            "c-1",
            &map(json!({"email": "A@Example.com", "age": 30, "tags": ["x"], "gone": null})),
        );

        let planned = planned.unwrap_or_else(|problems| panic!("{problems:?}"));
        assert_eq!(
            planned.identifiers,
            [
                (KEY_KIND.to_owned(), "c-1".to_owned()),
                ("email".to_owned(), "A@Example.com".to_owned())
            ]
        );
        assert_eq!(
            planned.traits,
            map(json!({"email": "A@Example.com", "age": 30, "tags": ["x"]}))
        );
    }

    #[test]
    fn a_missing_required_field_is_named_and_null_counts_as_missing() {
        for body in [json!({}), json!({"email": null})] {
            assert_eq!(
                problems(plan(Some(&definition()), "c", &map(body))),
                [("email".to_owned(), "missing_required")]
            );
        }
    }

    #[test]
    fn wrong_types_and_undeclared_fields_are_named_without_values() {
        let result = plan(
            Some(&definition()),
            "c",
            &map(json!({"email": "a@example.com", "age": "secret-age", "zeta": 1})),
        );

        let found = problems(result);

        assert_eq!(
            found,
            [
                ("age".to_owned(), "wrong_type"),
                ("zeta".to_owned(), "unplanned_property")
            ]
        );
        assert!(!format!("{found:?}").contains("secret-age"));
    }

    #[test]
    fn an_identifier_field_is_normalised_by_the_identifier_rules() {
        let result = plan(
            Some(&definition()),
            "c",
            &map(json!({"email": "not an address"})),
        );

        assert_eq!(problems(result), [("email".to_owned(), "invalid_email")]);
    }

    #[test]
    fn without_a_definition_only_the_key_is_required_and_keys_must_be_valid() {
        let planned = plan(None, "c", &map(json!({"plan": "pro"}))).unwrap();
        assert_eq!(planned.identifiers, [(KEY_KIND.to_owned(), "c".to_owned())]);
        assert_eq!(planned.traits, map(json!({"plan": "pro"})));
        assert!(plan(None, "c", &Map::new()).is_ok());

        assert_eq!(
            problems(plan(None, "c", &map(json!({"Bad-Key": 1})))),
            [("Bad-Key".to_owned(), "invalid_key")]
        );
    }
}
