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
    /// Whether it signs the message itself with the sender domain's DKIM key. The send pipeline
    /// opens that key only for a type that says so, and passes it in [`Outbound::dkim`](super::Outbound::dkim).
    pub signs_dkim: bool,
}

/// What one destination can do for a marketer, from its type and its configuration. The one
/// place the pickers and the erase job read it from.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Abilities {
    /// Delivers email.
    pub send: bool,
    /// Runs profile operations (an upsert or a remove).
    pub sync: bool,
    /// Declares the erase capability.
    pub erase: bool,
}

impl DestinationSpec {
    /// The abilities of a destination of this type with `config`. A type that carries the
    /// `profile_sync` channel is a manifest provider: its manifest decides what it does.
    pub fn abilities(&self, config: &Value) -> Abilities {
        let channels = self.capabilities.channels;
        if channels.contains(&"profile_sync") {
            let support = super::provider::profile_support(config);
            return Abilities {
                send: channels.contains(&"email") && support.email,
                sync: support.upsert || support.remove,
                erase: super::provider::erase_key_source(config).is_some(),
            };
        }
        Abilities {
            send: channels.contains(&"email"),
            sync: false,
            erase: self.capabilities.erase,
        }
    }
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
            signs_dkim: false,
        };

        assert_eq!(base, base);
        assert_ne!(
            base,
            Capabilities {
                erase: true,
                ..base
            }
        );
        assert_ne!(
            base,
            Capabilities {
                signs_dkim: true,
                ..base
            }
        );
    }

    fn provider_abilities(fixture: &str) -> Abilities {
        let config =
            serde_json::json!({"manifest": serde_json::from_str::<Value>(fixture).unwrap()});
        super::super::Registry::built_in()
            .get("provider")
            .unwrap()
            .spec()
            .abilities(&config)
    }

    #[test]
    fn a_plain_type_sends_by_its_email_channel_and_never_syncs() {
        let registry = super::super::Registry::built_in();
        let of = |kind: &str| {
            registry
                .get(kind)
                .unwrap()
                .spec()
                .abilities(&serde_json::json!({}))
        };

        assert_eq!(
            of("smtp"),
            Abilities {
                send: true,
                sync: false,
                erase: false
            }
        );
        assert_eq!(of("webhook"), Abilities::default());
    }

    #[test]
    fn a_provider_sends_when_its_manifest_has_send_and_syncs_when_it_has_a_profile_operation() {
        let email = provider_abilities(include_str!(
            "../../tests/fixtures/providers/bearer_timestamped.json"
        ));
        let profiles = provider_abilities(include_str!(
            "../../tests/fixtures/providers/profile_sync.json"
        ));

        assert_eq!(
            email,
            Abilities {
                send: true,
                sync: false,
                erase: false
            }
        );
        assert_eq!(
            profiles,
            Abilities {
                send: false,
                sync: true,
                erase: false
            }
        );
        assert_eq!(provider_abilities("{}"), Abilities::default());
    }
}
