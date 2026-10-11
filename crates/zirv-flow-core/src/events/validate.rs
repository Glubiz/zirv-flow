//! Validating an event against a schema and inferring a draft from an unplanned event
//! (architecture section 12.1). Reasons name a property and a code, never a value.

use chrono::{DateTime, NaiveDate};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::property::{EventMode, PropertyType, SchemaProperty, is_name};

const MAX_INFERRED_PROPERTIES: usize = 200;

/// Why a property failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasonCode {
    MissingRequired,
    WrongType,
    NotInEnum,
    UnplannedProperty,
}

impl ReasonCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MissingRequired => "missing_required",
            Self::WrongType => "wrong_type",
            Self::NotInEnum => "not_in_enum",
            Self::UnplannedProperty => "unplanned_property",
        }
    }
}

/// One failed check: the property and the code.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Reason {
    pub property: String,
    pub reason: ReasonCode,
}

/// The validation state stored with an accepted event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ValidationState {
    Valid,
    Flagged,
    Unplanned,
}

impl ValidationState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Valid => "valid",
            Self::Flagged => "flagged",
            Self::Unplanned => "unplanned",
        }
    }
}

/// Checks `properties` against `schema`. A JSON `null` counts as absent. Reasons come in
/// schema order, then unplanned properties in key order.
pub fn check_properties(properties: &Map<String, Value>, schema: &[SchemaProperty]) -> Vec<Reason> {
    let mut reasons = Vec::new();
    for declared in schema {
        let value = properties
            .get(&declared.name)
            .filter(|value| !value.is_null());
        let Some(value) = value else {
            if declared.required {
                reasons.push(Reason {
                    property: declared.name.clone(),
                    reason: ReasonCode::MissingRequired,
                });
            }
            continue;
        };
        if !has_type(declared.property_type, value) {
            reasons.push(Reason {
                property: declared.name.clone(),
                reason: ReasonCode::WrongType,
            });
        } else if let (Some(allowed), Value::String(text)) = (&declared.enum_values, value)
            && !allowed.contains(text)
        {
            reasons.push(Reason {
                property: declared.name.clone(),
                reason: ReasonCode::NotInEnum,
            });
        }
    }
    let mut unplanned: Vec<&String> = properties
        .keys()
        .filter(|key| schema.iter().all(|declared| &declared.name != *key))
        .collect();
    unplanned.sort();
    reasons.extend(unplanned.into_iter().map(|key| Reason {
        property: key.clone(),
        reason: ReasonCode::UnplannedProperty,
    }));
    reasons
}

pub(super) fn has_type(property_type: PropertyType, value: &Value) -> bool {
    match (property_type, value) {
        (PropertyType::String, Value::String(_))
        | (PropertyType::Boolean, Value::Bool(_))
        | (PropertyType::Object, Value::Object(_)) => true,
        (PropertyType::Number, Value::Number(number)) => {
            number.as_f64().is_some_and(f64::is_finite)
        }
        (PropertyType::Integer, Value::Number(number)) => {
            number.is_i64()
                || number.is_u64()
                || number
                    .as_f64()
                    .is_some_and(|float| float.is_finite() && float.fract() == 0.0)
        }
        (PropertyType::Timestamp, Value::String(text)) => {
            DateTime::parse_from_rfc3339(text).is_ok()
        }
        (PropertyType::Date, Value::String(text)) => {
            text.len() == 10 && NaiveDate::parse_from_str(text, "%Y-%m-%d").is_ok()
        }
        (PropertyType::StringList, Value::Array(items)) => items.iter().all(Value::is_string),
        _ => false,
    }
}

/// What the mode makes of the reasons: `None` means reject.
pub fn evaluate(mode: EventMode, reasons: &[Reason]) -> Option<ValidationState> {
    match (mode, reasons.is_empty()) {
        (EventMode::Block, false) => None,
        (EventMode::Warn, false) => Some(ValidationState::Flagged),
        _ => Some(ValidationState::Valid),
    }
}

/// A draft from the observed shape: string, number, boolean, string list or object, never
/// required and never PII. A value of any other shape (null, a mixed list) is left out.
pub fn infer_properties(properties: &Map<String, Value>) -> Vec<SchemaProperty> {
    let mut keys: Vec<&String> = properties.keys().collect();
    keys.sort();
    keys.into_iter()
        .filter(|key| is_name(key))
        .filter_map(|key| {
            let property_type = match &properties[key] {
                Value::String(_) => PropertyType::String,
                Value::Number(_) => PropertyType::Number,
                Value::Bool(_) => PropertyType::Boolean,
                Value::Array(items) if items.iter().all(Value::is_string) => {
                    PropertyType::StringList
                }
                Value::Object(_) => PropertyType::Object,
                _ => return None,
            };
            Some(SchemaProperty {
                name: key.clone(),
                property_type,
                required: false,
                enum_values: None,
                description: String::new(),
                pii: false,
            })
        })
        .take(MAX_INFERRED_PROPERTIES)
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn declared(name: &str, property_type: PropertyType, required: bool) -> SchemaProperty {
        SchemaProperty {
            name: name.to_owned(),
            property_type,
            required,
            enum_values: None,
            description: String::new(),
            pii: false,
        }
    }

    fn map(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap()
    }

    fn codes(reasons: &[Reason]) -> Vec<(&str, ReasonCode)> {
        reasons
            .iter()
            .map(|reason| (reason.property.as_str(), reason.reason))
            .collect()
    }

    #[test]
    fn a_matching_event_has_no_reasons() {
        let schema = [
            declared("total", PropertyType::Number, true),
            declared("at", PropertyType::Timestamp, false),
            declared("tags", PropertyType::StringList, false),
        ];

        let reasons = check_properties(
            &map(json!({"total": 9.5, "at": "2026-01-01T00:00:00Z", "tags": ["a"]})),
            &schema,
        );

        assert!(reasons.is_empty());
    }

    #[test]
    fn missing_wrong_enum_and_unplanned_are_named_without_values() {
        let mut tier = declared("tier", PropertyType::String, false);
        tier.enum_values = Some(vec!["free".into()]);
        let schema = [
            declared("total", PropertyType::Integer, true),
            declared("count", PropertyType::Number, true),
            tier,
        ];

        let reasons = check_properties(
            &map(json!({"count": "secret-value", "tier": "gold", "zeta": 1, "alpha": 2})),
            &schema,
        );

        assert_eq!(
            codes(&reasons),
            [
                ("total", ReasonCode::MissingRequired),
                ("count", ReasonCode::WrongType),
                ("tier", ReasonCode::NotInEnum),
                ("alpha", ReasonCode::UnplannedProperty),
                ("zeta", ReasonCode::UnplannedProperty),
            ]
        );
        assert!(
            !serde_json::to_string(&reasons)
                .unwrap()
                .contains("secret-value")
        );
    }

    #[test]
    fn null_counts_as_absent() {
        let schema = [
            declared("a", PropertyType::String, true),
            declared("b", PropertyType::String, false),
        ];

        let reasons = check_properties(&map(json!({"a": null, "b": null})), &schema);

        assert_eq!(codes(&reasons), [("a", ReasonCode::MissingRequired)]);
    }

    #[test]
    fn each_type_accepts_its_values_only() {
        let cases = [
            (PropertyType::String, json!("x"), json!(1)),
            (PropertyType::Number, json!(1.5), json!("1.5")),
            (PropertyType::Integer, json!(3), json!(3.5)),
            (PropertyType::Boolean, json!(false), json!("false")),
            (
                PropertyType::Timestamp,
                json!("2026-01-01T00:00:00+02:00"),
                json!("yesterday"),
            ),
            (PropertyType::Date, json!("2026-02-28"), json!("2026-02-30")),
            (PropertyType::StringList, json!(["a"]), json!(["a", 1])),
            (PropertyType::Object, json!({"k": 1}), json!([1])),
        ];
        for (property_type, good, bad) in cases {
            assert!(has_type(property_type, &good), "{property_type:?} {good}");
            assert!(!has_type(property_type, &bad), "{property_type:?} {bad}");
        }
        assert!(has_type(PropertyType::Integer, &json!(5.0)));
    }

    #[test]
    fn the_mode_decides_between_accepting_flagging_and_rejecting() {
        let none: Vec<Reason> = Vec::new();
        let some = vec![Reason {
            property: "a".into(),
            reason: ReasonCode::WrongType,
        }];

        assert_eq!(
            evaluate(EventMode::Block, &none),
            Some(ValidationState::Valid)
        );
        assert_eq!(evaluate(EventMode::Block, &some), None);
        assert_eq!(
            evaluate(EventMode::Warn, &some),
            Some(ValidationState::Flagged)
        );
        assert_eq!(
            evaluate(EventMode::Warn, &none),
            Some(ValidationState::Valid)
        );
        assert_eq!(
            evaluate(EventMode::Allow, &some),
            Some(ValidationState::Valid)
        );
    }

    #[test]
    fn inference_reads_the_shape_and_never_requires_or_flags() {
        let inferred = infer_properties(&map(json!({
            "b": true, "a": "x", "n": 3, "l": ["x"], "o": {"k": 1}, "mixed": [1, "x"], "gone": null
        })));

        let shape: Vec<(&str, PropertyType)> = inferred
            .iter()
            .map(|property| (property.name.as_str(), property.property_type))
            .collect();
        assert_eq!(
            shape,
            [
                ("a", PropertyType::String),
                ("b", PropertyType::Boolean),
                ("l", PropertyType::StringList),
                ("n", PropertyType::Number),
                ("o", PropertyType::Object),
            ]
        );
        assert!(
            inferred
                .iter()
                .all(|property| !property.required && !property.pii)
        );
    }

    #[test]
    fn state_names_are_stable() {
        assert_eq!(ValidationState::Valid.as_str(), "valid");
        assert_eq!(ValidationState::Flagged.as_str(), "flagged");
        assert_eq!(ValidationState::Unplanned.as_str(), "unplanned");
        assert_eq!(ReasonCode::UnplannedProperty.as_str(), "unplanned_property");
    }
}
