//! Masking of property values for display (architecture section 12.1): PII-flagged values
//! for viewers without access, and the stricter rule for reject samples.

use serde_json::{Map, Value};

use super::property::{PropertyType, SchemaProperty};
use super::validate::has_type;

const SAMPLE_STRING_CHARS: usize = 256;

/// Properties after masking, and the names whose values were replaced by `null`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Masked {
    pub properties: Map<String, Value>,
    pub masked: Vec<String>,
}

/// Replaces the values of PII-flagged properties with `null` unless `may_view_pii` is true.
pub fn mask(
    properties: &Map<String, Value>,
    schema: &[SchemaProperty],
    may_view_pii: bool,
) -> Masked {
    let mut result = Map::new();
    let mut masked = Vec::new();
    for (name, value) in properties {
        let pii = schema
            .iter()
            .any(|declared| &declared.name == name && declared.pii);
        if pii && !may_view_pii {
            result.insert(name.clone(), Value::Null);
            masked.push(name.clone());
        } else {
            result.insert(name.clone(), value.clone());
        }
    }
    masked.sort();
    Masked {
        properties: result,
        masked,
    }
}

/// The masked sample of a rejected event. A value stays only when the published schema
/// declares that property as non-PII and not an object, and the value has the declared type;
/// every other value, and every value of an event without a published schema, becomes
/// `null`. Strings that stay, nested ones included, are cut to 256 characters.
pub fn mask_sample(
    properties: &Map<String, Value>,
    published: Option<&[SchemaProperty]>,
) -> Masked {
    let mut result = Map::new();
    let mut masked = Vec::new();
    for (name, value) in properties {
        let visible = published.is_some_and(|schema| {
            schema.iter().any(|declared| {
                &declared.name == name
                    && !declared.pii
                    && declared.property_type != PropertyType::Object
                    && has_type(declared.property_type, value)
            })
        });
        if visible {
            result.insert(name.clone(), shorten(value));
        } else {
            result.insert(name.clone(), Value::Null);
            masked.push(name.clone());
        }
    }
    masked.sort();
    Masked {
        properties: result,
        masked,
    }
}

fn shorten(value: &Value) -> Value {
    match value {
        Value::String(text) => Value::String(text.chars().take(SAMPLE_STRING_CHARS).collect()),
        Value::Array(items) => Value::Array(items.iter().map(shorten).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| (key.clone(), shorten(item)))
                .collect(),
        ),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn declared(name: &str, property_type: PropertyType, pii: bool) -> SchemaProperty {
        SchemaProperty {
            name: name.to_owned(),
            property_type,
            required: false,
            enum_values: None,
            description: String::new(),
            pii,
        }
    }

    fn map(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap()
    }

    #[test]
    fn pii_values_are_nulled_for_viewers_without_access_only() {
        let schema = [
            declared("email", PropertyType::String, true),
            declared("plan", PropertyType::String, false),
        ];
        let properties = map(json!({"email": "a@b.co", "plan": "pro", "extra": 1}));

        let hidden = mask(&properties, &schema, false);
        let shown = mask(&properties, &schema, true);

        assert_eq!(
            Value::Object(hidden.properties),
            json!({"email": null, "plan": "pro", "extra": 1})
        );
        assert_eq!(hidden.masked, ["email"]);
        assert_eq!(shown.properties, properties);
        assert!(shown.masked.is_empty());
    }

    #[test]
    fn a_sample_keeps_only_values_the_schema_calls_non_pii() {
        let schema = [
            declared("total", PropertyType::Number, false),
            declared("email", PropertyType::String, true),
            declared("blob", PropertyType::Object, false),
        ];
        let properties = map(json!({
            "total": 12, "email": "a@b.co", "blob": {"k": "v"}, "surprise": "x"
        }));

        let sample = mask_sample(&properties, Some(&schema));

        assert_eq!(
            Value::Object(sample.properties),
            json!({"total": 12, "email": null, "blob": null, "surprise": null})
        );
        assert_eq!(sample.masked, ["blob", "email", "surprise"]);
    }

    #[test]
    fn a_value_of_the_wrong_type_is_masked_even_for_a_non_pii_property() {
        let schema = [
            declared("total", PropertyType::Number, false),
            declared("note", PropertyType::String, false),
        ];
        let properties = map(json!({"total": {"card": "4111"}, "note": "fine"}));

        let sample = mask_sample(&properties, Some(&schema));

        assert_eq!(
            Value::Object(sample.properties),
            json!({"total": null, "note": "fine"})
        );
        assert_eq!(sample.masked, ["total"]);
    }

    #[test]
    fn every_value_of_an_unplanned_event_is_masked() {
        let properties = map(json!({"a": 1, "b": "x"}));

        let sample = mask_sample(&properties, None);

        assert_eq!(
            Value::Object(sample.properties),
            json!({"a": null, "b": null})
        );
        assert_eq!(sample.masked, ["a", "b"]);
    }

    #[test]
    fn strings_that_stay_are_cut_to_256_characters() {
        let schema = [
            declared("note", PropertyType::String, false),
            declared("tags", PropertyType::StringList, false),
        ];
        let long = "é".repeat(300);
        let properties = map(json!({"note": long, "tags": [long]}));

        let sample = mask_sample(&properties, Some(&schema));

        assert_eq!(
            sample.properties["note"].as_str().unwrap().chars().count(),
            256
        );
        assert_eq!(
            sample.properties["tags"][0]
                .as_str()
                .unwrap()
                .chars()
                .count(),
            256
        );
    }
}
