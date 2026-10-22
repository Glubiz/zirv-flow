//! What a destination type declares about itself: the fields a marketer fills in and what it can
//! do. Field specs are the ones sources use, so the setup form renders both alike.

use serde_json::Value;

use crate::sources::{FieldError, FieldSpec};

/// A destination type, as the registry lists it.
#[derive(Debug)]
pub struct DestinationSpec {
    /// The stable id stored in `destinations.kind`, such as `smtp`.
    pub kind: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    /// Settings kept as plain configuration.
    pub config_fields: &'static [FieldSpec],
    /// Settings kept only in an encrypted secret. Every field here has `secret` set.
    pub credential_fields: &'static [FieldSpec],
    pub capabilities: Capabilities,
    /// Checks that span or refine the fields, run once every field has its declared type. It
    /// returns the settings that failed, naming each without repeating its value.
    pub config_check: Option<fn(&Value) -> Vec<FieldError>>,
}

/// What a destination type can do.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Capabilities {
    /// The channels it carries, such as `email`.
    pub channels: &'static [&'static str],
    /// Whether it passes the idempotency key on, so a retried delivery cannot duplicate.
    pub idempotency_key: bool,
    /// Whether it can deliver several messages in one call.
    pub batching: bool,
    /// Whether the type's destinations are rate limited by the shared bucket.
    pub rate_limit: bool,
    /// Whether it can erase a person's data at the remote system.
    pub erase: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_compare_field_by_field() {
        let base = Capabilities {
            channels: &["email"],
            idempotency_key: true,
            batching: false,
            rate_limit: true,
            erase: false,
        };

        assert_eq!(base, base);
        assert_ne!(
            base,
            Capabilities {
                erase: true,
                ..base
            }
        );
    }
}
