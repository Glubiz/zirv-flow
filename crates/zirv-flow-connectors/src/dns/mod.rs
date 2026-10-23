//! DNS TXT lookups for sending-domain verification (architecture 10.2).
//!
//! A [`DnsResolver`] answers one question: what TXT records does a name hold. The real one,
//! [`SystemResolver`], asks the operating system's configured name servers over plain DNS with
//! `hickory-resolver` (no TLS or DNSSEC feature, so neither OpenSSL nor aws-lc is linked). The
//! fixture resolver, behind `test-support`, serves a map, so tests need no network.
//!
//! Lookups do not go through the HTTP egress policy (`GuardedClient`). That policy governs
//! outbound connections to addresses a tenant supplies; a TXT lookup opens none, it sends the
//! name to the name servers the operator configured for the host. What a tenant can learn through
//! it is bounded by the callers: a sending domain must be a public name (`parse_domain` refuses
//! internal and special-use suffixes), and only TXT text of the expected record type is stored or
//! returned, rebuilt from its parsed fields (`redact_observed`). A CNAME in an answer's chain whose
//! target is not a public name (the same rule, `is_public_name`) voids the answer: the lookup
//! fails with [`DnsError::PrivateAlias`], which carries none of the target's text, so a public name
//! cannot be made to answer from a private zone and the caller can tell the alias from a missing
//! record. Architecture 16.2 records the reasoning.

use std::future::Future;
use std::pin::Pin;

use zirv_flow_core::sending::is_public_name;

mod system;

#[cfg(any(test, feature = "test-support"))]
mod fixture;

pub use system::SystemResolver;

#[cfg(any(test, feature = "test-support"))]
pub use fixture::FixtureResolver;

/// Why a lookup gave no answer. "The name holds no TXT record" is not an error: it is an empty
/// answer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DnsError {
    /// The name is not a DNS name.
    InvalidName,
    /// The name servers did not answer (a timeout, a refusal or a server failure). Retry later;
    /// the records' last known state says nothing.
    Unavailable,
    /// The name is an alias, directly or through a chain, to a name that is not public. The alias
    /// was not followed and nothing of the target is reported. Unlike [`Self::Unavailable`] this
    /// is an answer, not an outage: public receivers cannot resolve the record either.
    PrivateAlias,
}

impl std::fmt::Display for DnsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::InvalidName => "not a valid DNS name",
            Self::Unavailable => "the name servers did not answer",
            Self::PrivateAlias => "an alias to a non-public name, not followed",
        })
    }
}

impl std::error::Error for DnsError {}

/// Whether every CNAME target in an answer's chain is a public name. A resolver fails an answer
/// for which this is false with [`DnsError::PrivateAlias`].
pub(crate) fn chain_is_public<S: AsRef<str>>(targets: impl IntoIterator<Item = S>) -> bool {
    targets
        .into_iter()
        .all(|target| is_public_name(target.as_ref()))
}

/// A boxed lookup, so a resolver can be used as a trait object.
pub type Lookup<'a> = Pin<Box<dyn Future<Output = Result<Vec<String>, DnsError>> + Send + 'a>>;

/// Looks up TXT records.
pub trait DnsResolver: Send + Sync + std::fmt::Debug {
    /// The TXT records at `name`, one string per record with its character strings joined. An
    /// empty list when the name or its TXT records do not exist; [`DnsError::PrivateAlias`] when
    /// the name is an alias to a non-public name.
    fn lookup_txt<'a>(&'a self, name: &'a str) -> Lookup<'a>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_have_fixed_texts() {
        assert_eq!(DnsError::InvalidName.to_string(), "not a valid DNS name");
        assert_eq!(
            DnsError::Unavailable.to_string(),
            "the name servers did not answer"
        );
        assert_eq!(
            DnsError::PrivateAlias.to_string(),
            "an alias to a non-public name, not followed"
        );
    }

    #[tokio::test]
    async fn a_resolver_is_usable_as_a_trait_object() {
        let fixture = FixtureResolver::new();
        fixture.set("a.example.com", &["v=spf1 -all"]);
        let resolver: std::sync::Arc<dyn DnsResolver> = std::sync::Arc::new(fixture);

        let found = resolver.lookup_txt("a.example.com").await.unwrap();

        assert_eq!(found, ["v=spf1 -all"]);
    }
}
