//! What a source type declares about itself: the fields a marketer fills in and what it can do.
//! The setup wizard renders its form from these.

use super::validate::FieldError;
use crate::runtime::RateLimit;

/// A source type, as the registry lists it.
#[derive(Debug)]
pub struct SourceSpec {
    /// The stable id stored in `sources.kind`, such as `fake`.
    pub kind: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    /// Settings kept as plain configuration.
    pub config_fields: &'static [FieldSpec],
    /// Settings kept only in an encrypted secret. Every field here has `secret` set.
    pub credential_fields: &'static [FieldSpec],
    pub capabilities: Capabilities,
    /// The request rate the source type tolerates, enforced per process by the runtime;
    /// `None` is unlimited.
    pub rate_limit: Option<RateLimit>,
    /// A check of the whole configuration that the field specs cannot express, run by
    /// `validate_config` once every field has its declared type. Pure, and its reasons never
    /// repeat a value: an `http` source checks its manifest here.
    pub config_check: Option<fn(&serde_json::Value) -> Vec<FieldError>>,
}

/// What a source type can do.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Capabilities {
    pub incremental: bool,
    pub full_refresh: bool,
    /// Whether it reports deleted records.
    pub emits_deletes: bool,
}

/// One setting of a source type.
#[derive(Debug)]
pub struct FieldSpec {
    pub key: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    pub kind: FieldKind,
    pub required: bool,
    /// True only in [`SourceSpec::credential_fields`]: shown as a password input, never read back.
    pub secret: bool,
}

/// The value type of a setting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FieldKind {
    Text,
    /// An absolute `http` or `https` URL without credentials or a fragment.
    Url,
    /// As `Url`, but `https` only: `http` is accepted only in a build with `test-support`, where
    /// tests serve over loopback. Shown as a `url` field.
    HttpsUrl,
    Number,
    Bool,
    /// One of the listed values.
    Select(&'static [&'static str]),
    /// A JSON object. The setup form shows it as text the person edits.
    Json,
}

impl FieldKind {
    /// The name the API and the form use for the kind.
    pub fn name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Url | Self::HttpsUrl => "url",
            Self::Number => "number",
            Self::Bool => "bool",
            Self::Select(_) => "select",
            Self::Json => "json",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_kind_has_a_stable_name() {
        assert_eq!(FieldKind::Text.name(), "text");
        assert_eq!(FieldKind::Url.name(), "url");
        assert_eq!(FieldKind::Number.name(), "number");
        assert_eq!(FieldKind::Bool.name(), "bool");
        assert_eq!(FieldKind::Select(&["a"]).name(), "select");
        assert_eq!(FieldKind::Json.name(), "json");
    }
}
