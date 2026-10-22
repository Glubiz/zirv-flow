//! What is wrong with a template document: a fixed code, how bad it is, and where.
//!
//! The style follows `zirv_flow_core::flow::issue`: codes are constants, an issue holds a JSON
//! pointer into the document. An issue also names the merge field it concerns, which comes from
//! the marketer's own template text, never from a person or an event.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use zirv_flow_core::people::TraitType;

/// The fixed issue codes.
pub mod code {
    // Blocking: the document cannot be published.
    pub const UNSUPPORTED_SCHEMA: &str = "unsupported_schema";
    pub const UNSUPPORTED_CHANNEL: &str = "unsupported_channel";
    pub const UNSUPPORTED_MODE: &str = "unsupported_mode";
    pub const MISSING_CONTENT: &str = "missing_content";
    pub const CONTENT_TOO_LARGE: &str = "content_too_large";
    pub const SYNTAX_ERROR: &str = "syntax_error";
    pub const UNKNOWN_MERGE_FIELD: &str = "unknown_merge_field";
    pub const MISSING_UNSUBSCRIBE: &str = "missing_unsubscribe";
    pub const MISSING_SENDER: &str = "missing_sender";
    pub const INVALID_JSON_BODY: &str = "invalid_json_body";
    pub const TEMPLATE_IN_KEY: &str = "template_in_key";
    pub const DYNAMIC_MERGE_FIELD: &str = "dynamic_merge_field";
    pub const INVALID_HEADER_NAME: &str = "invalid_header_name";
    pub const TOO_MANY_HEADERS: &str = "too_many_headers";
    pub const HEADERS_TOO_LARGE: &str = "headers_too_large";
    // Warnings: the document can be published.
    pub const MISSING_LANG: &str = "missing_lang";
    pub const INSECURE_LINK: &str = "insecure_link";
    pub const HTML_NEAR_CLIP: &str = "html_near_clip";

    /// Every code that blocks publishing.
    pub const BLOCKING: [&str; 15] = [
        UNSUPPORTED_SCHEMA,
        UNSUPPORTED_CHANNEL,
        UNSUPPORTED_MODE,
        MISSING_CONTENT,
        CONTENT_TOO_LARGE,
        SYNTAX_ERROR,
        UNKNOWN_MERGE_FIELD,
        MISSING_UNSUBSCRIBE,
        MISSING_SENDER,
        INVALID_JSON_BODY,
        TEMPLATE_IN_KEY,
        DYNAMIC_MERGE_FIELD,
        INVALID_HEADER_NAME,
        TOO_MANY_HEADERS,
        HEADERS_TOO_LARGE,
    ];

    /// Every code that only warns.
    pub const WARNING: [&str; 3] = [MISSING_LANG, INSECURE_LINK, HTML_NEAR_CLIP];
}

/// Whether an issue stops publishing.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Blocking,
    Warning,
}

/// The severity of a code; an unknown code is blocking.
pub fn severity_of(code: &str) -> Severity {
    if code::WARNING.contains(&code) {
        Severity::Warning
    } else {
        Severity::Blocking
    }
}

/// One problem: a fixed code, its severity, a JSON pointer into the document and, for a merge
/// field problem, the field path (for example `person.first_name`).
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Issue {
    pub code: &'static str,
    pub severity: Severity,
    pub at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
}

impl Issue {
    pub(crate) fn new(code: &'static str, at: impl Into<String>) -> Self {
        Self {
            code,
            severity: severity_of(code),
            at: at.into(),
            field: None,
        }
    }

    pub(crate) fn with_field(mut self, field: impl Into<String>) -> Self {
        self.field = Some(field.into());
        self
    }

    /// Whether this issue stops publishing.
    pub fn is_blocking(&self) -> bool {
        self.severity == Severity::Blocking
    }
}

/// The merge fields a template may use, passed in by the caller from the field registry and the
/// event catalog (architecture 11.4).
#[derive(Clone, Debug, Default)]
pub struct FieldCatalog {
    /// Trait key to its registry type.
    pub person: BTreeMap<String, TraitType>,
    /// Property names of the trigger event.
    pub event: BTreeSet<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_code_has_one_severity() {
        for code in code::BLOCKING {
            assert_eq!(severity_of(code), Severity::Blocking, "{code}");
        }
        for code in code::WARNING {
            assert_eq!(severity_of(code), Severity::Warning, "{code}");
        }
        let all: BTreeSet<&str> = code::BLOCKING.into_iter().chain(code::WARNING).collect();
        assert_eq!(all.len(), code::BLOCKING.len() + code::WARNING.len());
    }

    #[test]
    fn issue_serialises_without_empty_field() {
        let issue = Issue::new(code::MISSING_LANG, "/email/lang");
        assert!(!issue.is_blocking());
        assert_eq!(
            serde_json::to_string(&issue).unwrap(),
            r#"{"code":"missing_lang","severity":"warning","at":"/email/lang"}"#
        );
        let issue = Issue::new(code::UNKNOWN_MERGE_FIELD, "/email/html").with_field("person.x");
        assert!(issue.is_blocking());
        assert_eq!(issue.field.as_deref(), Some("person.x"));
    }
}
