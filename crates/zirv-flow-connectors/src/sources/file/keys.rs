//! The key of a record within its stream.

use serde_json::{Map, Value};

use super::super::record_key::key_of;

/// The key of `record`: the value of the primary key field, or the values of several joined as
/// a JSON array of strings (unambiguous whatever the values hold). With no primary key, or
/// when a part is missing or empty, the key is `row:<ordinal>`, which holds only while the file
/// is not reordered.
pub(super) fn record_key(
    primary_key: &[String],
    record: &Map<String, Value>,
    ordinal: u64,
) -> String {
    key_of(primary_key, record).unwrap_or_else(|| format!("row:{ordinal}"))
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
    fn a_key_is_the_primary_key_value_and_a_keyless_record_is_its_ordinal() {
        assert_eq!(key(&["id"], json!({"id": "u-1"}), 3), "u-1");
        assert_eq!(
            key(&["a", "b"], json!({"a": "x,y", "b": 2}), 1),
            r#"["x,y","2"]"#
        );
        assert_eq!(key(&[], json!({"id": "u"}), 7), "row:7");
        assert_eq!(key(&["id"], json!({}), 8), "row:8");
        assert_eq!(key(&["id"], json!({"id": ""}), 9), "row:9");
        assert_eq!(key(&["a", "b"], json!({"a": "x"}), 12), "row:12");
    }
}
