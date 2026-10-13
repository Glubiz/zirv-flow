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
    /// Checks that span or refine the fields (a host's format, a port's range), run by
    /// [`validate_config`](super::validate_config) once every field has its declared type. It
    /// returns the settings that failed, naming each without repeating its value.
    pub config_check: Option<fn(&serde_json::Value) -> Vec<FieldError>>,
}

/// What a source type can do.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Capabilities {
    /// Whether records arrive by push (an inbound webhook) instead of being read. A push source
    /// has no schedule and no run.
    pub push: bool,
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
    /// Multi-line text, such as a PEM certificate.
    LongText,
    /// A list of named SQL queries (database sources): `[{"name": "...", "sql": "..."}]`.
    Queries,
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
            Self::LongText => "long_text",
            Self::Queries => "queries",
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
        assert_eq!(FieldKind::LongText.name(), "long_text");
        assert_eq!(FieldKind::Queries.name(), "queries");
    }
}
