//! The key of a record within its stream.

use serde_json::{Map, Value};

use super::super::record_key::key_of;

/// The key of `record`: the value of the primary key field, or the values of several joined as
/// a JSON array of strings (unambiguous whatever the values hold). With no primary key, or
/// when a part is missing or empty, the key is U+001F then `row:<ordinal>`, which holds only
/// while the file is not reordered. A real key that starts with U+001F gets one more, so no
/// real value equals a fallback and `row:3` stays an ordinary id.
pub(super) fn record_key(
    primary_key: &[String],
    record: &Map<String, Value>,
    ordinal: u64,
) -> String {
    match key_of(primary_key, record) {
        Some(key) if key.starts_with(SEPARATOR) => format!("{SEPARATOR}{key}"),
        Some(key) => key,
        None => format!("{SEPARATOR}row:{ordinal}"),
    }
}

/// Marks a keyless record's fallback key; a real key never begins with exactly one of it.
const SEPARATOR: char = '\u{1f}';

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
        assert_eq!(key(&[], json!({"id": "u"}), 7), "\u{1f}row:7");
        assert_eq!(key(&["id"], json!({}), 8), "\u{1f}row:8");
        assert_eq!(key(&["id"], json!({"id": ""}), 9), "\u{1f}row:9");
        assert_eq!(key(&["a", "b"], json!({"a": "x"}), 12), "\u{1f}row:12");
    }

    #[test]
    fn keyless_row_key_does_not_collide_with_a_real_row_n_id() {
        assert_ne!(
            key(&["id"], json!({"id": "row:3"}), 5),
            key(&["id"], json!({}), 3)
        );
        assert_eq!(key(&["id"], json!({"id": "row:3"}), 5), "row:3");
        // A real id that already starts with the marker is escaped, never a fallback.
        assert_ne!(
            key(&["id"], json!({"id": "\u{1f}row:3"}), 5),
            key(&["id"], json!({}), 3)
        );
        assert_ne!(key(&["id"], json!({}), 3), key(&["id"], json!({}), 4));
    }
}
