//! The sample kept of a record a sync run refused: only the values a reader may see.

use serde_json::{Map, Value};

use super::model::{Mapping, Target};

/// The longest string a sample keeps.
pub const SAMPLE_STRING_CHARS: usize = 256;

/// A record's top-level fields after masking, and the names whose values were replaced by
/// `null`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RejectSample {
    pub properties: Map<String, Value>,
    pub masked: Vec<String>,
}

/// Masks a refused record for display, the way a rejected event's sample is masked: a field's
/// value stays only when `mapping` sends that field to a trait for which `plain` is true (the
/// field registry marks it non-PII), and the value is a string, a number or a boolean. Every
/// other value is `null` and its field is listed in `masked`; so is every field the mapping
/// does not use, and an identifier, list or ignored field. Kept strings are cut to 256
/// characters. Field names are kept; nested fields are not looked into.
pub fn reject_sample(
    mapping: &Mapping,
    data: &Value,
    plain: impl Fn(&str) -> bool,
) -> RejectSample {
    let mut properties = Map::new();
    let mut masked = Vec::new();
    let Some(object) = data.as_object() else {
        return RejectSample { properties, masked };
    };
    for (name, value) in object {
        let clean_name = clean(name);
        let visible = mapping.columns.iter().any(|rule| {
            &rule.column == name && matches!(&rule.target, Target::Trait { key } if plain(key))
        }) && matches!(value, Value::String(_) | Value::Number(_) | Value::Bool(_));
        if visible {
            properties.insert(clean_name, shorten(value));
        } else {
            properties.insert(clean_name.clone(), Value::Null);
            masked.push(clean_name);
        }
    }
    masked.sort();
    RejectSample { properties, masked }
}

/// Replaces the NUL characters of `text`, which PostgreSQL refuses in `jsonb`: one odd record
/// must not make its own reject impossible to store.
fn clean(text: &str) -> String {
    text.replace('\0', "\u{FFFD}")
}

fn shorten(value: &Value) -> Value {
    match value {
        Value::String(text) => Value::String(clean(
            &text.chars().take(SAMPLE_STRING_CHARS).collect::<String>(),
        )),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use uuid::Uuid;

    use super::super::model::ColumnRule;
    use super::*;

    fn rule(column: &str, target: Target) -> ColumnRule {
        ColumnRule {
            column: column.into(),
            target,
            transforms: Vec::new(),
        }
    }

    fn mapping() -> Mapping {
        Mapping {
            key_column: Some("id".into()),
            columns: vec![
                rule(
                    "email",
                    Target::Identifier {
                        kind: "email".into(),
                    },
                ),
                rule("plan", Target::Trait { key: "plan".into() }),
                rule("ssn", Target::Trait { key: "ssn".into() }),
                rule("tags", Target::Trait { key: "plan".into() }),
                rule(
                    "joined",
                    Target::List {
                        list_id: Uuid::nil(),
                    },
                ),
                rule("note", Target::Ignore),
            ],
        }
    }

    fn plain(key: &str) -> bool {
        key == "plan"
    }

    #[test]
    fn only_values_mapped_to_non_pii_traits_stay() {
        let data = json!({
            "id": "k-1", "email": "ada@example.com", "plan": "pro", "ssn": "123",
            "tags": ["a"], "joined": true, "note": "hi", "extra": 4
        });

        let sample = reject_sample(&mapping(), &data, plain);

        assert_eq!(sample.properties["plan"], json!("pro"));
        for masked in ["id", "email", "ssn", "tags", "joined", "note", "extra"] {
            assert_eq!(sample.properties[masked], Value::Null, "{masked}");
        }
        assert_eq!(
            sample.masked,
            ["email", "extra", "id", "joined", "note", "ssn", "tags"]
        );
        let shown = serde_json::to_string(&sample.properties).unwrap();
        assert!(!shown.contains("ada@example.com") && !shown.contains("123"));
    }

    #[test]
    fn numbers_and_booleans_stay_and_long_strings_are_cut() {
        let mapping = Mapping {
            key_column: None,
            columns: vec![
                rule("a", Target::Trait { key: "plan".into() }),
                rule("b", Target::Trait { key: "plan".into() }),
                rule("c", Target::Trait { key: "plan".into() }),
            ],
        };
        let data = json!({"a": 7, "b": false, "c": "x".repeat(300)});

        let sample = reject_sample(&mapping, &data, plain);

        assert_eq!(sample.properties["a"], json!(7));
        assert_eq!(sample.properties["b"], json!(false));
        assert_eq!(
            sample.properties["c"].as_str().unwrap().chars().count(),
            256
        );
        assert!(sample.masked.is_empty());
    }

    #[test]
    fn a_nul_in_a_name_or_a_kept_value_becomes_a_replacement_character() {
        let mapping = Mapping {
            key_column: None,
            columns: vec![rule("pl\0an", Target::Trait { key: "plan".into() })],
        };
        let data = json!({"pl\0an": "a\0b", "ot\0her": "x"});

        let sample = reject_sample(&mapping, &data, plain);

        assert_eq!(sample.properties["pl\u{FFFD}an"], json!("a\u{FFFD}b"));
        assert_eq!(sample.masked, ["ot\u{FFFD}her"]);
        let stored = serde_json::to_string(&sample.properties).unwrap();
        assert!(!stored.contains("\\u0000"), "{stored}");
    }

    #[test]
    fn a_record_that_is_not_an_object_has_an_empty_sample() {
        let sample = reject_sample(&mapping(), &json!([1]), plain);

        assert!(sample.properties.is_empty() && sample.masked.is_empty());
    }
}
