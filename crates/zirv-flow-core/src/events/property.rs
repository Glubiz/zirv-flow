//! Event names, schema properties and the modes that govern them (architecture section 12.1).

use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::ValidationError;

const MAX_NAME_CHARS: usize = 128;
const MAX_PROPERTIES: usize = 200;
const MAX_ENUM_VALUES: usize = 100;
const MAX_ENUM_VALUE_CHARS: usize = 128;
const MAX_DESCRIPTION_CHARS: usize = 500;

/// An event name: 1 to 128 characters with no control characters, and not `.` or `..`, which
/// cannot be told apart from path segments in a URL.
pub fn validate_event_name(name: &str) -> Result<(), ValidationError> {
    if is_name(name) && name != "." && name != ".." {
        Ok(())
    } else {
        Err(ValidationError::new(
            "name",
            "must be 1 to 128 characters without control characters, and not . or ..",
        ))
    }
}

/// An event property name: 1 to 128 characters without control characters.
pub fn validate_property_name(name: &str) -> Result<(), ValidationError> {
    if is_name(name) {
        Ok(())
    } else {
        Err(ValidationError::new(
            "property",
            "must be 1 to 128 characters without control characters",
        ))
    }
}

pub(super) fn is_name(name: &str) -> bool {
    let length = name.chars().count();
    (1..=MAX_NAME_CHARS).contains(&length) && !name.chars().any(char::is_control)
}

/// The type of a schema property.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PropertyType {
    String,
    Number,
    Integer,
    Boolean,
    /// RFC 3339.
    Timestamp,
    /// `YYYY-MM-DD`.
    Date,
    StringList,
    /// Any JSON object; not validated inside.
    Object,
}

impl PropertyType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Number => "number",
            Self::Integer => "integer",
            Self::Boolean => "boolean",
            Self::Timestamp => "timestamp",
            Self::Date => "date",
            Self::StringList => "string_list",
            Self::Object => "object",
        }
    }
}

impl fmt::Display for PropertyType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for PropertyType {
    type Err = ValidationError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "string" => Ok(Self::String),
            "number" => Ok(Self::Number),
            "integer" => Ok(Self::Integer),
            "boolean" => Ok(Self::Boolean),
            "timestamp" => Ok(Self::Timestamp),
            "date" => Ok(Self::Date),
            "string_list" => Ok(Self::StringList),
            "object" => Ok(Self::Object),
            _ => Err(ValidationError::new("type", "is not a known property type")),
        }
    }
}

/// One property of an event schema.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SchemaProperty {
    pub name: String,
    #[serde(rename = "type")]
    pub property_type: PropertyType,
    #[serde(default)]
    pub required: bool,
    /// The allowed values; for string properties only.
    #[serde(default, rename = "enum", skip_serializing_if = "Option::is_none")]
    pub enum_values: Option<Vec<String>>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub pii: bool,
}

/// Checks a property list: names are top-level keys of 1 to 128 characters and unique,
/// enums are non-empty lists on string properties.
pub fn validate_properties(properties: &[SchemaProperty]) -> Result<(), ValidationError> {
    if properties.len() > MAX_PROPERTIES {
        return Err(ValidationError::new(
            "properties",
            "must list at most 200 properties",
        ));
    }
    let mut seen = HashSet::new();
    for property in properties {
        if !is_name(&property.name) {
            return Err(ValidationError::new(
                "properties",
                "each name must be 1 to 128 characters without control characters",
            ));
        }
        if !seen.insert(property.name.as_str()) {
            return Err(ValidationError::new(
                "properties",
                "each name must appear once",
            ));
        }
        if property.description.chars().count() > MAX_DESCRIPTION_CHARS {
            return Err(ValidationError::new(
                "properties",
                "each description must be at most 500 characters",
            ));
        }
        if let Some(values) = &property.enum_values {
            let valid = property.property_type == PropertyType::String
                && !values.is_empty()
                && values.len() <= MAX_ENUM_VALUES
                && values
                    .iter()
                    .all(|value| value.chars().count() <= MAX_ENUM_VALUE_CHARS);
            if !valid {
                return Err(ValidationError::new(
                    "properties",
                    "an enum must list 1 to 100 short values and belongs to a string property",
                ));
            }
        }
    }
    Ok(())
}

/// How a schema treats an event that breaks it.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventMode {
    /// Accept and keep the reasons without flagging the event.
    Allow,
    /// Accept and flag the event.
    Warn,
    /// Reject the event.
    Block,
}

impl EventMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Warn => "warn",
            Self::Block => "block",
        }
    }
}

impl FromStr for EventMode {
    type Err = ValidationError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "allow" => Ok(Self::Allow),
            "warn" => Ok(Self::Warn),
            "block" => Ok(Self::Block),
            _ => Err(ValidationError::new("mode", "must be allow, warn or block")),
        }
    }
}

/// What the workspace does with an event whose name has no published schema.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UnplannedPolicy {
    /// Accept it and draft a schema from the observed shape.
    Draft,
    Reject,
}

impl UnplannedPolicy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Reject => "reject",
        }
    }
}

impl FromStr for UnplannedPolicy {
    type Err = ValidationError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "draft" => Ok(Self::Draft),
            "reject" => Ok(Self::Reject),
            _ => Err(ValidationError::new("unplanned", "must be draft or reject")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn property(name: &str, property_type: PropertyType) -> SchemaProperty {
        SchemaProperty {
            name: name.to_owned(),
            property_type,
            required: false,
            enum_values: None,
            description: String::new(),
            pii: false,
        }
    }

    #[test]
    fn names_are_1_to_128_characters_without_control_characters() {
        assert!(validate_event_name("Order Completed").is_ok());
        assert!(validate_event_name("...").is_ok());
        assert!(validate_event_name(&"é".repeat(128)).is_ok());
        for bad in ["", &"a".repeat(129), "a\nb", "a\u{0}b", ".", ".."] {
            assert!(validate_event_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn property_lists_need_unique_valid_names_and_sensible_enums() {
        assert!(validate_properties(&[property("a", PropertyType::Integer)]).is_ok());
        assert!(validate_properties(&[property("", PropertyType::Integer)]).is_err());
        assert!(
            validate_properties(&[
                property("a", PropertyType::Integer),
                property("a", PropertyType::String)
            ])
            .is_err()
        );
        let mut enumerated = property("tier", PropertyType::String);
        enumerated.enum_values = Some(vec!["free".into(), "pro".into()]);
        assert!(validate_properties(std::slice::from_ref(&enumerated)).is_ok());
        enumerated.enum_values = Some(Vec::new());
        assert!(validate_properties(std::slice::from_ref(&enumerated)).is_err());
        let mut numeric = property("n", PropertyType::Number);
        numeric.enum_values = Some(vec!["1".into()]);
        assert!(validate_properties(&[numeric]).is_err());
        let many: Vec<SchemaProperty> = (0..201)
            .map(|index| property(&format!("p{index}"), PropertyType::String))
            .collect();
        assert!(validate_properties(&many).is_err());
    }

    #[test]
    fn a_property_round_trips_through_json_with_type_and_enum_keys() {
        let json = serde_json::json!({"name": "tier", "type": "string", "enum": ["a"]});

        let parsed: SchemaProperty = serde_json::from_value(json).unwrap();

        assert_eq!(parsed.enum_values, Some(vec!["a".to_owned()]));
        assert!(!parsed.required && !parsed.pii);
        let back = serde_json::to_value(&parsed).unwrap();
        assert_eq!(back["type"], "string");
        assert_eq!(back["enum"], serde_json::json!(["a"]));
        assert_eq!(PropertyType::StringList.as_str(), "string_list");
    }

    #[test]
    fn property_types_round_trip_through_text() {
        for property_type in [
            PropertyType::String,
            PropertyType::Number,
            PropertyType::Integer,
            PropertyType::Boolean,
            PropertyType::Timestamp,
            PropertyType::Date,
            PropertyType::StringList,
            PropertyType::Object,
        ] {
            assert_eq!(
                property_type.as_str().parse::<PropertyType>().unwrap(),
                property_type
            );
        }
        assert!("list".parse::<PropertyType>().is_err());
    }

    #[test]
    fn modes_and_policies_parse_from_their_names() {
        for mode in [EventMode::Allow, EventMode::Warn, EventMode::Block] {
            assert_eq!(mode.as_str().parse::<EventMode>().unwrap(), mode);
        }
        for policy in [UnplannedPolicy::Draft, UnplannedPolicy::Reject] {
            assert_eq!(policy.as_str().parse::<UnplannedPolicy>().unwrap(), policy);
        }
        assert!("strict".parse::<EventMode>().is_err());
        assert!("ignore".parse::<UnplannedPolicy>().is_err());
    }
}
