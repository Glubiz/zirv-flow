//! The destination registry: the one place destination types are listed.

use super::destination::ErasedDestination;
use super::spec::DestinationSpec;

/// The destination types a build can run.
#[derive(Clone)]
pub struct Registry {
    destinations: Vec<&'static dyn ErasedDestination>,
}

impl Registry {
    /// Every destination type this build ships.
    pub fn built_in() -> Self {
        Self {
            destinations: super::ALL.to_vec(),
        }
    }

    pub fn get(&self, kind: &str) -> Option<&'static dyn ErasedDestination> {
        self.destinations
            .iter()
            .copied()
            .find(|destination| destination.spec().kind == kind)
    }

    /// The specs of every registered type, in registration order.
    pub fn specs(&self) -> Vec<&'static DestinationSpec> {
        self.destinations
            .iter()
            .map(|destination| destination.spec())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use hashbrown::HashSet;

    use super::*;

    #[test]
    fn kinds_are_unique_and_every_credential_has_a_secret_field_and_no_config_field_is_secret() {
        let specs = Registry::built_in().specs();

        let kinds: HashSet<_> = specs.iter().map(|spec| spec.kind).collect();
        assert_eq!(kinds.len(), specs.len());
        for spec in specs {
            assert!(spec.credential_fields.iter().any(|field| field.secret));
            assert!(spec.config_fields.iter().all(|field| !field.secret));
        }
    }

    #[test]
    fn the_webhook_is_registered_in_every_build() {
        let registry = Registry::built_in();

        assert_eq!(registry.get("webhook").unwrap().spec().kind, "webhook");
    }

    #[test]
    fn smtp_is_registered_in_every_build_and_signs() {
        let registry = Registry::built_in();

        let smtp = registry.get("smtp").unwrap().spec();

        assert!(smtp.capabilities.signs_dkim);
        assert!(smtp.capabilities.channels.contains(&"email"));
        assert!(
            Registry::built_in()
                .specs()
                .iter()
                .filter(|spec| spec.kind != "smtp")
                .all(|spec| !spec.capabilities.signs_dkim)
        );
    }

    #[test]
    fn an_unknown_kind_is_not_found() {
        assert!(Registry::built_in().get("no-such-kind").is_none());
    }

    #[test]
    fn the_provider_is_registered_in_every_build_and_takes_email() {
        let provider = Registry::built_in().get("provider").unwrap().spec();

        assert!(provider.capabilities.channels.contains(&"email"));
        assert!(provider.capabilities.idempotency_key);
        assert!(!provider.capabilities.signs_dkim);
    }

    #[cfg(feature = "test-support")]
    #[test]
    fn the_fake_is_registered_with_test_support() {
        let registry = Registry::built_in();

        assert_eq!(registry.get("fake").unwrap().spec().kind, "fake");
        assert!(registry.specs().iter().any(|spec| spec.kind == "fake"));
    }

    #[cfg(not(feature = "test-support"))]
    #[test]
    fn only_the_webhook_smtp_and_provider_are_registered_in_a_production_build() {
        let kinds: Vec<_> = Registry::built_in()
            .specs()
            .iter()
            .map(|spec| spec.kind)
            .collect();

        assert_eq!(kinds, ["webhook", "smtp", "provider"]);
        assert!(Registry::built_in().get("fake").is_none());
    }

    #[test]
    fn each_destination_module_is_named_once_in_its_registry_line() {
        let production = include_str!("mod.rs");

        assert_eq!(production.matches("fake::Fake").count(), 1);
        assert_eq!(production.matches("webhook::Webhook").count(), 1);
        assert_eq!(production.matches("smtp::Smtp").count(), 1);
        assert_eq!(
            production.matches("provider::ProviderDestination").count(),
            1
        );
    }

    #[test]
    fn every_module_file_of_this_directory_is_declared_once() {
        let production = include_str!("mod.rs");
        for module in [
            "context",
            "destination",
            "dkim",
            "error",
            "fake",
            "provider",
            "registry",
            "retry_after",
            "smtp",
            "spec",
            "validate",
            "webhook",
        ] {
            assert_eq!(
                production.matches(&format!("mod {module};")).count(),
                1,
                "{module}"
            );
        }
    }
}
