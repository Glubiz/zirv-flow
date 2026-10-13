//! The source registry: the one place source types are listed.

use super::source::ErasedSource;
use super::spec::SourceSpec;

/// The source types a build can run.
#[derive(Clone)]
pub struct Registry {
    sources: Vec<&'static dyn ErasedSource>,
}

impl Registry {
    /// Every source type this build ships.
    pub fn built_in() -> Self {
        Self {
            sources: super::ALL.to_vec(),
        }
    }

    pub fn get(&self, kind: &str) -> Option<&'static dyn ErasedSource> {
        self.sources
            .iter()
            .copied()
            .find(|source| source.spec().kind == kind)
    }

    /// The specs of every registered type, in registration order.
    pub fn specs(&self) -> Vec<&'static SourceSpec> {
        self.sources.iter().map(|source| source.spec()).collect()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn kinds_are_unique_and_every_credential_field_is_secret_and_no_config_field_is() {
        let specs = Registry::built_in().specs();

        let kinds: BTreeSet<_> = specs.iter().map(|spec| spec.kind).collect();
        assert_eq!(kinds.len(), specs.len());
        for spec in specs {
            assert!(spec.credential_fields.iter().all(|field| field.secret));
            assert!(spec.config_fields.iter().all(|field| !field.secret));
        }
    }

    #[test]
    fn an_unknown_kind_is_not_found() {
        assert!(Registry::built_in().get("no-such-kind").is_none());
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
    fn the_fake_is_not_registered_without_test_support() {
        assert!(Registry::built_in().get("fake").is_none());
    }

    #[test]
    fn a_source_module_is_reached_only_through_its_registry_line() {
        let production = include_str!("mod.rs");
        for module in ["fake::Fake", "postgres::Postgres"] {
            let mentions = production.matches(module).count();

            assert_eq!(mentions, 1, "one registry line names {module}");
        }
    }
}
