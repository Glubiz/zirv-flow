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
//! returned (`redact_observed`). Architecture 16.2 records the reasoning.

use std::future::Future;
use std::pin::Pin;

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
}

impl std::fmt::Display for DnsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::InvalidName => "not a valid DNS name",
            Self::Unavailable => "the name servers did not answer",
        })
    }
}

impl std::error::Error for DnsError {}

/// A boxed lookup, so a resolver can be used as a trait object.
pub type Lookup<'a> = Pin<Box<dyn Future<Output = Result<Vec<String>, DnsError>> + Send + 'a>>;

/// Looks up TXT records.
pub trait DnsResolver: Send + Sync + std::fmt::Debug {
    /// The TXT records at `name`, one string per record with its character strings joined. An
    /// empty list when the name or its TXT records do not exist.
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
