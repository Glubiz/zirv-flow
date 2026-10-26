//! What is wrong with a template document: a fixed code, how bad it is, and where.
//!
//! The style follows `zirv_flow_core::flow::issue`: codes are constants, an issue holds a JSON
//! pointer into the document. An issue also names the merge field it concerns, which comes from
//! the marketer's own template text, never from a person or an event.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use zirv_flow_core::events::PropertyType;
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
    pub const BODY_TOO_LARGE: &str = "body_too_large";
    pub const MJML_PARSE_ERROR: &str = "mjml_parse_error";
    pub const MJML_INCLUDE: &str = "mjml_include";
    pub const MJML_TOO_DEEP: &str = "mjml_too_deep";
    pub const MJML_IN_RAW_HTML: &str = "mjml_in_raw_html";
    pub const MARKUP_IN_PLAIN_FIELD: &str = "markup_in_plain_field";
    pub const AMBIGUOUS_CONTENT: &str = "ambiguous_content";
    pub const BLOCKS_LIMIT: &str = "blocks_limit";
    pub const DUPLICATE_BLOCK_ID: &str = "duplicate_block_id";
    pub const INVALID_BLOCK_ID: &str = "invalid_block_id";
    pub const INVALID_COLUMNS: &str = "invalid_columns";
    pub const INVALID_SETTING: &str = "invalid_setting";
    pub const INVALID_URL: &str = "invalid_url";
    pub const SANITIZER_REJECTED: &str = "sanitizer_rejected";
    pub const CONDITION_TOO_LARGE: &str = "condition_too_large";
    pub const BROKEN_ASSET_REFERENCE: &str = "broken_asset_reference";
    pub const BROKEN_THEME_REFERENCE: &str = "broken_theme_reference";
    pub const BROKEN_SNIPPET_REFERENCE: &str = "broken_snippet_reference";
    pub const SNIPPET_SECTION: &str = "snippet_section";
    pub const SNIPPET_NESTED: &str = "snippet_nested";
    pub const SNIPPET_CONDITION: &str = "snippet_condition";
    pub const NOT_CONVERTIBLE: &str = "not_convertible";
    // #307 removes this when the evaluator supplies condition results.
    pub const CONDITION_UNSUPPORTED: &str = "condition_unsupported";
    // Warnings: the document can be published.
    pub const MISSING_LANG: &str = "missing_lang";
    pub const INSECURE_LINK: &str = "insecure_link";
    pub const HTML_NEAR_CLIP: &str = "html_near_clip";
    pub const SAMPLE_RENDER_FAILED: &str = "sample_render_failed";
    pub const URL_FROM_MERGE_FIELD: &str = "url_from_merge_field";
    pub const IMAGE_WITHOUT_ALT: &str = "image_without_alt";
    pub const SUBJECT_TOO_LONG: &str = "subject_too_long";
    pub const PREHEADER_TOO_LONG: &str = "preheader_too_long";
    pub const LOW_CONTRAST: &str = "low_contrast";
    pub const SMS_MANY_SEGMENTS: &str = "sms_many_segments";

    /// Every code that blocks publishing.
    pub const BLOCKING: [&str; 38] = [
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
        BODY_TOO_LARGE,
        MJML_PARSE_ERROR,
        MJML_INCLUDE,
        MJML_TOO_DEEP,
        MJML_IN_RAW_HTML,
        MARKUP_IN_PLAIN_FIELD,
        AMBIGUOUS_CONTENT,
        BLOCKS_LIMIT,
        DUPLICATE_BLOCK_ID,
        INVALID_BLOCK_ID,
        INVALID_COLUMNS,
        INVALID_SETTING,
        INVALID_URL,
        SANITIZER_REJECTED,
        CONDITION_TOO_LARGE,
        BROKEN_ASSET_REFERENCE,
        BROKEN_THEME_REFERENCE,
        BROKEN_SNIPPET_REFERENCE,
        SNIPPET_SECTION,
        SNIPPET_NESTED,
        SNIPPET_CONDITION,
        NOT_CONVERTIBLE,
        CONDITION_UNSUPPORTED,
    ];

    /// Every code that only warns.
    pub const WARNING: [&str; 10] = [
        MISSING_LANG,
        INSECURE_LINK,
        HTML_NEAR_CLIP,
        SAMPLE_RENDER_FAILED,
        IMAGE_WITHOUT_ALT,
        SUBJECT_TOO_LONG,
        PREHEADER_TOO_LONG,
        URL_FROM_MERGE_FIELD,
        LOW_CONTRAST,
        SMS_MANY_SEGMENTS,
    ];
}

/// Whether an issue stops publishing.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
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
    /// A short explanation, for a problem the code alone does not explain (an unknown filter
    /// name, the error of a sample render). It comes from the template text or the sandbox, never
    /// from a person or an event.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// The wire form of an [`Issue`]: the code is text until it is matched to a fixed one.
#[derive(Deserialize)]
struct IssueWire {
    code: String,
    at: String,
    #[serde(default)]
    field: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

impl<'de> Deserialize<'de> for Issue {
    /// An issue read back from the render host. The code must be one of the fixed codes, so a
    /// response carrying an unknown one is unreadable, not trusted; the severity is the code's.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = IssueWire::deserialize(deserializer)?;
        let code = code::BLOCKING
            .iter()
            .chain(code::WARNING.iter())
            .copied()
            .find(|known| *known == wire.code)
            .ok_or_else(|| serde::de::Error::custom("is not a known issue code"))?;
        Ok(Self {
            code,
            severity: severity_of(code),
            at: wire.at,
            field: wire.field,
            message: wire.message,
        })
    }
}

impl Issue {
    pub(crate) fn new(code: &'static str, at: impl Into<String>) -> Self {
        Self {
            code,
            severity: severity_of(code),
            at: at.into(),
            field: None,
            message: None,
        }
    }

    pub(crate) fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
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
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct FieldCatalog {
    /// Trait key to its registry type.
    pub person: BTreeMap<String, TraitType>,
    /// Trigger event property name to its schema type. A path below an `Object` property
    /// (`event.order.total`) is accepted; below any other type it is unknown.
    pub event: BTreeMap<String, PropertyType>,
}

impl FieldCatalog {
    /// The catalog plus the reserved `person.email`, a string whatever the registry holds.
    pub fn with_reserved_email(mut self) -> Self {
        self.person
            .insert(crate::context::RESERVED_EMAIL.to_owned(), TraitType::String);
        self
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

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
    fn an_issue_round_trips_and_an_unknown_code_is_refused() {
        let issue = Issue::new(code::UNKNOWN_MERGE_FIELD, "/email/html")
            .with_field("person.x")
            .with_message("m");
        let json = serde_json::to_string(&issue).unwrap();
        assert_eq!(serde_json::from_str::<Issue>(&json).unwrap(), issue);
        let unknown = r#"{"code":"made_up","severity":"blocking","at":"/x"}"#;
        assert!(serde_json::from_str::<Issue>(unknown).is_err());
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
