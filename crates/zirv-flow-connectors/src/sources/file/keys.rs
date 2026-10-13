//! The key of a record within its stream.

use serde_json::{Map, Value};

/// A field value as key text: a string as it is, a number or a boolean as its JSON text.
/// Nothing else is a key, and neither is an empty string.
fn part(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        other @ (Value::Number(_) | Value::Bool(_)) => Some(other.to_string()),
        _ => None,
    }
}

/// The key of `record`: the value of the primary key field, or the values of several joined as
/// a JSON array of strings (unambiguous whatever the values hold). With no primary key, or
/// when a part is missing or empty, the key is `row:<ordinal>`, which holds only while the file
/// is not reordered.
pub(super) fn record_key(
    primary_key: &[String],
    record: &Map<String, Value>,
    ordinal: u64,
) -> String {
    let parts: Option<Vec<String>> = primary_key
        .iter()
        .map(|field| part(record.get(field)))
        .collect();
    match parts {
        Some(mut parts) if parts.len() == 1 => parts.remove(0),
        Some(parts) if !parts.is_empty() => {
            serde_json::to_string(&parts).unwrap_or_else(|_| format!("row:{ordinal}"))
        }
        _ => format!("row:{ordinal}"),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn key(fields: &[&str], value: Value, ordinal: u64) -> String {
        let fields: Vec<String> = fields.iter().map(|field| (*field).to_owned()).collect();
        record_key(&fields, value.as_object().unwrap(), ordinal)
    }

    #[test]
    fn one_field_is_the_key_as_it_is() {
        assert_eq!(key(&["id"], json!({"id": "u-1"}), 3), "u-1");
        assert_eq!(key(&["id"], json!({"id": 42}), 3), "42");
        assert_eq!(key(&["id"], json!({"id": true}), 3), "true");
    }

    #[test]
    fn several_fields_are_a_json_array_of_strings() {
        assert_eq!(
            key(&["a", "b"], json!({"a": "x,y", "b": 2}), 1),
            r#"["x,y","2"]"#
        );
        assert_ne!(
            key(&["a", "b"], json!({"a": "x", "b": "y,z"}), 1),
            key(&["a", "b"], json!({"a": "x,y", "b": "z"}), 1)
        );
    }

    #[test]
    fn no_primary_key_or_a_missing_part_gives_the_row_ordinal() {
        assert_eq!(key(&[], json!({"id": "u"}), 7), "row:7");
        assert_eq!(key(&["id"], json!({}), 8), "row:8");
        assert_eq!(key(&["id"], json!({"id": ""}), 9), "row:9");
        assert_eq!(key(&["id"], json!({"id": null}), 10), "row:10");
        assert_eq!(key(&["id"], json!({"id": {"x": 1}}), 11), "row:11");
        assert_eq!(key(&["a", "b"], json!({"a": "x"}), 12), "row:12");
    }
}
