//! What is wrong with a manifest: a code and a JSON pointer, never a value.

use std::fmt;

/// The most issues one validation reports; the rest are dropped.
pub(super) const MAX_ISSUES: usize = 50;

/// Codes. Each is a stable machine-readable string the editor turns into a sentence.
pub mod code {
    /// A required key is missing.
    pub const REQUIRED: &str = "required";
    /// A value is of the wrong JSON type.
    pub const INVALID_TYPE: &str = "invalid_type";
    /// A value is not one of the accepted choices.
    pub const INVALID_VALUE: &str = "invalid_value";
    /// A number is outside its range.
    pub const OUT_OF_RANGE: &str = "out_of_range";
    /// A text is shorter or longer than allowed.
    pub const INVALID_LENGTH: &str = "invalid_length";
    pub const INVALID_URL: &str = "invalid_url";
    pub const UNKNOWN_FIELD: &str = "unknown_field";
    /// A script-like key, or a template marker in a value.
    pub const EXPRESSION_NOT_SUPPORTED: &str = "expression_not_supported";
    pub const UNSUPPORTED_VERSION: &str = "unsupported_version";
    pub const MANIFEST_TOO_LARGE: &str = "manifest_too_large";
    pub const TOO_MANY_STREAMS: &str = "too_many_streams";
    pub const TOO_MANY_FIELDS: &str = "too_many_fields";
    pub const TOO_DEEP: &str = "too_deep";
    pub const TOO_MANY_HEADERS: &str = "too_many_headers";
    pub const FORBIDDEN_HEADER: &str = "forbidden_header";
    pub const INVALID_HEADER: &str = "invalid_header";
    pub const TOO_MANY_QUERY: &str = "too_many_query";
    pub const PATH_INVALID: &str = "path_invalid";
    pub const INVALID_POINTER: &str = "invalid_pointer";
    pub const DUPLICATE_STREAM: &str = "duplicate_stream";
    pub const DUPLICATE_FIELD: &str = "duplicate_field";
    pub const INVALID_FIELD_NAME: &str = "invalid_field_name";
    pub const PRIMARY_KEY_EMPTY: &str = "primary_key_empty";
    pub const PRIMARY_KEY_UNKNOWN_FIELD: &str = "primary_key_unknown_field";
    pub const CURSOR_FIELD_UNKNOWN: &str = "cursor_field_unknown";
    /// The `mapping` section is not a mapping.
    pub const INVALID_MAPPING: &str = "invalid_mapping";
    /// A mapping rule targets a list, whose ids are workspace data.
    pub const LIST_TARGET_NOT_ALLOWED: &str = "list_target_not_allowed";
    /// The `auth.credential` schema breaks a rule of credential schemas.
    pub const INVALID_CREDENTIAL_SCHEMA: &str = "invalid_credential_schema";
    /// The `auth.credential` schema lacks a field the auth type reads.
    pub const CREDENTIAL_FIELD_MISSING: &str = "credential_field_missing";

    /// Every code of this module. Mapping codes (`no_identifier`, `unknown_column`, ...) come
    /// from `zirv_flow_core::mapping::MappingError::code` and are not listed.
    pub const ALL: &[&str] = &[
        REQUIRED,
        INVALID_TYPE,
        INVALID_VALUE,
        OUT_OF_RANGE,
        INVALID_LENGTH,
        INVALID_URL,
        UNKNOWN_FIELD,
        EXPRESSION_NOT_SUPPORTED,
        UNSUPPORTED_VERSION,
        MANIFEST_TOO_LARGE,
        TOO_MANY_STREAMS,
        TOO_MANY_FIELDS,
        TOO_DEEP,
        TOO_MANY_HEADERS,
        FORBIDDEN_HEADER,
        INVALID_HEADER,
        TOO_MANY_QUERY,
        PATH_INVALID,
        INVALID_POINTER,
        DUPLICATE_STREAM,
        DUPLICATE_FIELD,
        INVALID_FIELD_NAME,
        PRIMARY_KEY_EMPTY,
        PRIMARY_KEY_UNKNOWN_FIELD,
        CURSOR_FIELD_UNKNOWN,
        INVALID_MAPPING,
        LIST_TARGET_NOT_ALLOWED,
        INVALID_CREDENTIAL_SCHEMA,
        CREDENTIAL_FIELD_MISSING,
    ];
}

/// One problem: a stable code and the JSON pointer of the key it concerns. A pointer holds the
/// keys of the manifest (an unknown key is named by its own pointer) and never a value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Issue {
    pub code: &'static str,
    pub path: String,
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at {}", self.code, self.path)
    }
}

/// Appends `key` to the pointer `parent` (RFC 6901 escaping), cutting a long key so a pointer
/// stays small.
pub(super) fn child(parent: &str, key: &str) -> String {
    const MAX_KEY_CHARS: usize = 64;
    let mut path = String::with_capacity(parent.len() + key.len() + 1);
    path.push_str(parent);
    path.push('/');
    for ch in key.chars().take(MAX_KEY_CHARS) {
        match ch {
            '~' => path.push_str("~0"),
            '/' => path.push_str("~1"),
            other => path.push(other),
        }
    }
    path
}

pub(super) fn index(parent: &str, position: usize) -> String {
    format!("{parent}/{position}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pointer_escapes_its_keys_and_cuts_a_long_one() {
        assert_eq!(child("/a", "b/c~d"), "/a/b~1c~0d");
        assert_eq!(index("/streams", 3), "/streams/3");
        assert_eq!(child("", &"x".repeat(500)).len(), 1 + 64);
    }

    #[test]
    fn display_names_the_code_and_the_path_only() {
        let issue = Issue {
            code: code::UNKNOWN_FIELD,
            path: "/streams/0/zzz".to_owned(),
        };

        assert_eq!(issue.to_string(), "unknown_field at /streams/0/zzz");
    }

    #[test]
    fn the_code_list_has_no_duplicates() {
        let mut codes = code::ALL.to_vec();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), code::ALL.len());
    }
}
