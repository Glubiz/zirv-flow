//! PostgreSQL stores any JSON number and any nesting in `jsonb`, but prints them back in ways
//! `serde_json` cannot always read: a number near the largest double comes back as 309 digits
//! that overflow, and the stored item wraps a submitted value in further levels that can pass
//! the parser's depth limit of 128. A value that cannot be read back would jam the drain that
//! applies it, so one outside these bounds is refused before it reaches the store.

use serde_json::{Map, Value};

/// Numbers of this size or beyond are refused. The largest double is about `1.8e308`, and the
/// decimal text PostgreSQL prints for the digits near it overflows `serde_json`'s reader.
pub const MAX_NUMBER: f64 = 1e308;

/// The deepest nesting a submitted value may have. The stored item adds a few levels around it
/// and `serde_json` reads 128.
pub const MAX_DEPTH: usize = 100;

/// Why a value cannot be stored readably.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Unreadable {
    /// A number at or beyond [`MAX_NUMBER`] in size.
    NumberOutOfRange,
    /// Arrays and objects nested deeper than [`MAX_DEPTH`].
    TooDeep,
}

impl Unreadable {
    /// The code a refusal carries, wherever one is reported.
    pub fn code(self) -> &'static str {
        match self {
            Self::NumberOutOfRange => "number_out_of_range",
            Self::TooDeep => "too_deep",
        }
    }
}

/// The first reason `value` cannot be read back once stored, if any.
pub fn find_unreadable(value: &Value) -> Option<Unreadable> {
    walk(value, 0)
}

/// [`find_unreadable`] for the members of an object.
pub fn find_unreadable_in_members(members: &Map<String, Value>) -> Option<Unreadable> {
    members.values().find_map(|item| walk(item, 1))
}

fn walk(value: &Value, depth: usize) -> Option<Unreadable> {
    match value {
        Value::Number(number) => number
            .as_f64()
            .filter(|n| n.abs() >= MAX_NUMBER)
            .map(|_| Unreadable::NumberOutOfRange),
        Value::Array(items) => {
            if depth >= MAX_DEPTH {
                return Some(Unreadable::TooDeep);
            }
            items.iter().find_map(|item| walk(item, depth + 1))
        }
        Value::Object(map) => {
            if depth >= MAX_DEPTH {
                return Some(Unreadable::TooDeep);
            }
            map.values().find_map(|item| walk(item, depth + 1))
        }
        Value::Null | Value::Bool(_) | Value::String(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn nested(levels: usize) -> Value {
        (0..levels).fold(json!(1), |inner, _| json!([inner]))
    }

    #[test]
    fn ordinary_values_are_readable() {
        assert_eq!(
            find_unreadable(&json!({"a": [1, 2.5, -3e300, "x", null, true], "b": {"c": 9e307}})),
            None
        );
    }

    #[test]
    fn a_number_at_or_near_the_double_limit_is_out_of_range() {
        for text in ["1.7976931348623157e308", "-1.7976931348623157e308", "1e308"] {
            let value: Value = serde_json::from_str(&format!("{{\"x\": [{text}]}}")).unwrap();
            assert_eq!(find_unreadable(&value), Some(Unreadable::NumberOutOfRange));
        }
    }

    #[test]
    fn nesting_past_the_limit_is_too_deep() {
        assert_eq!(find_unreadable(&nested(MAX_DEPTH)), None);
        assert_eq!(
            find_unreadable(&nested(MAX_DEPTH + 1)),
            Some(Unreadable::TooDeep)
        );
    }
}
