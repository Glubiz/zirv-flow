//! The key of a record within its stream.

use serde_json::{Map, Value};

use super::super::record_key::key_of;

/// The key of `record`: the value of the primary key field, or the values of several joined as
/// a JSON array of strings (unambiguous whatever the values hold). With no primary key, or
/// when a part is missing or empty, the key is `row:<ordinal>`, which holds only while the file
/// is not reordered. A real key that looks like a fallback (`row:<n>` after any number of U+001F)
/// gets one more U+001F, so no real value equals a fallback; every other real key is unchanged.
pub(super) fn record_key(
    primary_key: &[String],
    record: &Map<String, Value>,
    ordinal: u64,
) -> String {
    match key_of(primary_key, record) {
        Some(key) if looks_like_fallback(&key) => format!("{ESCAPE}{key}"),
        Some(key) => key,
        None => format!("row:{ordinal}"),
    }
}

/// Prefixed to a real key that looks like a fallback; the fallback itself never starts with it.
const ESCAPE: char = '\u{1f}';

/// Whether `key` matches `^\u{1f}*row:[0-9]+$`.
fn looks_like_fallback(key: &str) -> bool {
    key.trim_start_matches(ESCAPE)
        .strip_prefix("row:")
        .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
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

    #[test]
    fn a_real_key_that_looks_like_a_fallback_is_escaped_and_no_other_key_changes() {
        let real = |id: &str, ordinal| key(&["id"], json!({ "id": id }), ordinal);
        let fallback_3 = key(&["id"], json!({}), 3);
        assert_eq!(fallback_3, "row:3");
        assert_ne!(real("row:3", 5), fallback_3);
        assert_eq!(real("row:3", 5), "\u{1f}row:3");
        assert_ne!(real("\u{1f}row:3", 5), real("row:3", 5));
        assert_eq!(real("\u{1f}row:3", 5), "\u{1f}\u{1f}row:3");
        assert_eq!(real("\u{1f}abc", 5), "\u{1f}abc");
        assert_eq!(real("row:3a", 5), "row:3a");
        assert_eq!(real("rows:3", 5), "rows:3");
        assert_eq!(real("row:", 5), "row:");
        assert_ne!(key(&["id"], json!({}), 3), key(&["id"], json!({}), 4));
    }
}
