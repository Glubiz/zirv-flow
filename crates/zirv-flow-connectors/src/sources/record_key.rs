//! The key of a record within its stream, from the stream's primary key. Shared by the sources
//! that read JSON records (`file` and `http`).

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
/// a JSON array of strings (unambiguous whatever the values hold). `None` when there is no
/// primary key or a part is missing or empty; the source decides what a record without a key is.
pub(super) fn key_of(primary_key: &[String], record: &Map<String, Value>) -> Option<String> {
    let parts: Option<Vec<String>> = primary_key
        .iter()
        .map(|field| part(record.get(field)))
        .collect();
    match parts {
        Some(mut parts) if parts.len() == 1 => Some(parts.remove(0)),
        Some(parts) if !parts.is_empty() => serde_json::to_string(&parts).ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn key(fields: &[&str], value: Value) -> Option<String> {
        let fields: Vec<String> = fields.iter().map(|field| (*field).to_owned()).collect();
        key_of(&fields, value.as_object().unwrap())
    }

    #[test]
    fn one_field_is_the_key_as_it_is() {
        assert_eq!(key(&["id"], json!({"id": "u-1"})).as_deref(), Some("u-1"));
        assert_eq!(key(&["id"], json!({"id": 42})).as_deref(), Some("42"));
        assert_eq!(key(&["id"], json!({"id": true})).as_deref(), Some("true"));
    }

    #[test]
    fn several_fields_are_a_json_array_of_strings() {
        assert_eq!(
            key(&["a", "b"], json!({"a": "x,y", "b": 2})).as_deref(),
            Some(r#"["x,y","2"]"#)
        );
        assert_ne!(
            key(&["a", "b"], json!({"a": "x", "b": "y,z"})),
            key(&["a", "b"], json!({"a": "x,y", "b": "z"}))
        );
    }

    #[test]
    fn no_primary_key_or_a_missing_part_has_no_key() {
        assert_eq!(key(&[], json!({"id": "u"})), None);
        assert_eq!(key(&["id"], json!({})), None);
        assert_eq!(key(&["id"], json!({"id": ""})), None);
        assert_eq!(key(&["id"], json!({"id": null})), None);
        assert_eq!(key(&["id"], json!({"id": {"x": 1}})), None);
        assert_eq!(key(&["a", "b"], json!({"a": "x"})), None);
    }
}
