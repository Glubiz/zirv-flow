//! What is wrong with a definition: a code and a JSON pointer, never a value from it.

use std::fmt;

use serde::Serialize;

/// The fixed issue codes.
pub mod code {
    pub const UNSUPPORTED_VERSION: &str = "unsupported_version";
    pub const MALFORMED: &str = "malformed";
    pub const UNKNOWN_KEY: &str = "unknown_key";
    pub const UNKNOWN_TRAIT: &str = "unknown_trait";
    pub const UNKNOWN_EVENT: &str = "unknown_event";
    pub const UNKNOWN_PROPERTY: &str = "unknown_property";
    pub const UNKNOWN_LIST: &str = "unknown_list";
    pub const UNKNOWN_SEGMENT: &str = "unknown_segment";
    pub const UNKNOWN_TOPIC: &str = "unknown_topic";
    /// A consent dimension the workspace does not have, or has archived.
    pub const UNKNOWN_DIMENSION: &str = "unknown_dimension";
    pub const INVALID_IDENTIFIER_KIND: &str = "invalid_identifier_kind";
    pub const OPERATOR_NOT_ALLOWED: &str = "operator_not_allowed";
    pub const VALUE_TYPE_MISMATCH: &str = "value_type_mismatch";
    pub const VALUE_MISSING: &str = "value_missing";
    pub const VALUE_NOT_ALLOWED: &str = "value_not_allowed";
    pub const AGGREGATE_NOT_ALLOWED: &str = "aggregate_not_allowed";
    pub const INVALID_DURATION: &str = "invalid_duration";
    pub const WINDOW_TOO_LONG: &str = "window_too_long";
    pub const INVALID_RANGE: &str = "invalid_range";
    pub const TOO_DEEP: &str = "too_deep";
    pub const TOO_MANY_NODES: &str = "too_many_nodes";
    pub const TOO_MANY_VALUES: &str = "too_many_values";
    pub const VALUE_TOO_LONG: &str = "value_too_long";
    /// A leaf that reads the message being sent, which only a consent policy has.
    pub const POLICY_ONLY: &str = "policy_only";

    /// Every code.
    pub const ALL: [&str; 24] = [
        UNSUPPORTED_VERSION,
        MALFORMED,
        UNKNOWN_KEY,
        UNKNOWN_TRAIT,
        UNKNOWN_EVENT,
        UNKNOWN_PROPERTY,
        UNKNOWN_LIST,
        UNKNOWN_SEGMENT,
        UNKNOWN_TOPIC,
        UNKNOWN_DIMENSION,
        INVALID_IDENTIFIER_KIND,
        OPERATOR_NOT_ALLOWED,
        VALUE_TYPE_MISMATCH,
        VALUE_MISSING,
        VALUE_NOT_ALLOWED,
        AGGREGATE_NOT_ALLOWED,
        INVALID_DURATION,
        WINDOW_TOO_LONG,
        INVALID_RANGE,
        TOO_DEEP,
        TOO_MANY_NODES,
        TOO_MANY_VALUES,
        VALUE_TOO_LONG,
        POLICY_ONLY,
    ];
}

/// One problem: where (a JSON pointer into the definition) and what (a fixed code).
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Issue {
    pub pointer: String,
    pub code: &'static str,
}

impl Issue {
    pub fn new(pointer: impl Into<String>, code: &'static str) -> Self {
        Self {
            pointer: pointer.into(),
            code,
        }
    }
}

impl fmt::Display for Issue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.pointer, self.code)
    }
}

/// Appends one JSON pointer segment, escaping `~` and `/`.
pub fn pointer_push(pointer: &str, segment: &str) -> String {
    let mut next = String::with_capacity(pointer.len() + segment.len() + 1);
    next.push_str(pointer);
    next.push('/');
    for character in segment.chars() {
        match character {
            '~' => next.push_str("~0"),
            '/' => next.push_str("~1"),
            other => next.push(other),
        }
    }
    next
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pointer_segment_is_escaped() {
        assert_eq!(pointer_push("/a", "b/c~d"), "/a/b~1c~0d");
    }

    #[test]
    fn the_codes_are_unique_snake_case_and_displayed_with_the_pointer() {
        let mut seen = std::collections::BTreeSet::new();
        for text in code::ALL {
            assert!(seen.insert(text));
            assert!(text.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'));
        }
        assert_eq!(
            Issue::new("/all/0", code::MALFORMED).to_string(),
            "/all/0: malformed"
        );
    }
}
