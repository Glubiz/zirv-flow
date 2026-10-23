//! The real resolver: `hickory-resolver` on the operating system's name servers.

use std::time::Duration;

use hickory_resolver::TokioResolver;
use hickory_resolver::proto::rr::{RData, RecordType};

use super::{DnsError, DnsResolver, Lookup, chain_is_public, is_public_name};

/// How long one name server gets to answer, and how many tries a lookup makes. A verification
/// run makes three lookups, so a dead server delays it by well under a minute.
const TIMEOUT: Duration = Duration::from_secs(5);
const ATTEMPTS: usize = 2;
/// The most TXT records read from one answer. A name with more is not a sending domain's.
const MAX_RECORDS: usize = 64;
/// The most alias hops walked when looking for an alias into a private zone.
const MAX_HOPS: usize = 8;

/// Plain DNS over UDP and TCP to the servers in `/etc/resolv.conf` (or the Windows registry).
pub struct SystemResolver {
    resolver: TokioResolver,
}

impl std::fmt::Debug for SystemResolver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SystemResolver")
    }
}

impl SystemResolver {
    /// Reads the system configuration. Fails when the system names no usable name server.
    pub fn new() -> Result<Self, DnsError> {
        let mut builder = TokioResolver::builder_tokio().map_err(|_| DnsError::Unavailable)?;
        let options = builder.options_mut();
        options.timeout = TIMEOUT;
        options.attempts = ATTEMPTS;
        // A re-check must see DNS as it is now, not as a cached answer held it.
        options.cache_size = 0;
        // The CNAME chain is read from the answer to check each target, so keep the intermediate
        // records rather than relying on the default.
        options.preserve_intermediates = true;
        let resolver = builder.build().map_err(|_| DnsError::Unavailable)?;
        Ok(Self { resolver })
    }
}

/// Whether the alias chain that starts at `name` leaves the public namespace. `next` gives the
/// target of the alias at a name, or `None` when the name is no alias or cannot be asked. The walk
/// stops at the first non-public target, so it never reads past it.
async fn chain_leaves_public<F, Fut>(name: &str, mut next: F) -> bool
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Option<String>>,
{
    let mut at = name.to_owned();
    for _ in 0..MAX_HOPS {
        match next(at).await {
            Some(target) if !is_public_name(&target) => return true,
            Some(target) => at = target,
            None => return false,
        }
    }
    false
}

/// Whether `name` is made of labels of 1 to 63 bytes and is at most 253 bytes, so a malformed name
/// is refused before it reaches the wire.
fn is_dns_name(name: &str) -> bool {
    let name = name.strip_suffix('.').unwrap_or(name);
    !name.is_empty()
        && name.len() <= 253
        && name.split('.').all(|label| (1..=63).contains(&label.len()))
}

impl DnsResolver for SystemResolver {
    fn lookup_txt<'a>(&'a self, name: &'a str) -> Lookup<'a> {
        Box::pin(async move {
            if !is_dns_name(name) {
                return Err(DnsError::InvalidName);
            }
            // A trailing dot makes the name absolute, so no search domain is appended.
            let absolute = if name.ends_with('.') {
                name.to_owned()
            } else {
                format!("{name}.")
            };
            let outcome = self.txt(&absolute).await;
            // When the answer carries no records (no TXT, a missing name or a failed lookup), the
            // alias chain is read on its own, so a private alias gives the same outcome whatever
            // the private target holds or does. Otherwise the target would be an oracle.
            if outcome.as_ref().is_ok_and(Vec::is_empty) || outcome.is_err() {
                let leaves = chain_leaves_public(&absolute, |at| self.cname_of(at)).await;
                if leaves {
                    return Err(DnsError::PrivateAlias);
                }
            }
            outcome
        })
    }
}

impl SystemResolver {
    /// The target of the alias at `name`, if there is one and the name servers say so.
    async fn cname_of(&self, name: String) -> Option<String> {
        let lookup = self
            .resolver
            .lookup(name.as_str(), RecordType::CNAME)
            .await
            .ok()?;
        lookup
            .answers()
            .iter()
            .find_map(|record| match &record.data {
                RData::CNAME(cname) => Some(cname.0.to_string()),
                _ => None,
            })
    }

    async fn txt(&self, absolute: &str) -> Result<Vec<String>, DnsError> {
        match self.resolver.txt_lookup(absolute).await {
            Ok(lookup) => {
                let answers = lookup.answers();
                // A CNAME in the chain that leaves the public namespace voids the answer.
                let targets = answers.iter().filter_map(|record| match &record.data {
                    RData::CNAME(cname) => Some(cname.0.to_string()),
                    _ => None,
                });
                if !chain_is_public(targets) {
                    return Err(DnsError::PrivateAlias);
                }
                Ok(answers
                    .iter()
                    .filter_map(|record| match &record.data {
                        RData::TXT(txt) => Some(
                            txt.txt_data
                                .iter()
                                .map(|part| String::from_utf8_lossy(part))
                                .collect::<String>(),
                        ),
                        _ => None,
                    })
                    .take(MAX_RECORDS)
                    .collect())
            }
            Err(error) if error.is_no_records_found() => Ok(Vec::new()),
            Err(_) => Err(DnsError::Unavailable),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_invalid_name_is_an_error_not_a_panic() {
        let Ok(resolver) = SystemResolver::new() else {
            // A build machine with no name server configured has nothing to test against.
            return;
        };
        let name = format!("{}.example.com", "a".repeat(70));

        assert_eq!(resolver.lookup_txt(&name).await, Err(DnsError::InvalidName));
    }

    fn aliases(pairs: &[(&str, &str)]) -> impl FnMut(String) -> std::future::Ready<Option<String>> {
        let map: Vec<(String, String)> = pairs
            .iter()
            .map(|(from, to)| ((*from).to_owned(), (*to).to_owned()))
            .collect();
        move |name| {
            std::future::ready(
                map.iter()
                    .find(|(from, _)| *from == name)
                    .map(|(_, to)| to.clone()),
            )
        }
    }

    #[tokio::test]
    async fn a_chain_leaves_the_public_namespace_at_its_first_private_target() {
        let step = aliases(&[
            ("a.example.com.", "hop.provider.example.net."),
            ("hop.provider.example.net.", "x.secret.corp."),
        ]);
        assert!(chain_leaves_public("a.example.com.", step).await);
        let step = aliases(&[("a.example.com.", "dmarc.secret.corp.")]);
        assert!(chain_leaves_public("a.example.com.", step).await);
    }

    #[tokio::test]
    async fn a_public_or_absent_chain_does_not_leave() {
        let step = aliases(&[("a.example.com.", "hop.provider.example.net.")]);
        assert!(!chain_leaves_public("a.example.com.", step).await);
        assert!(!chain_leaves_public("b.example.com.", aliases(&[])).await);
        let looping = aliases(&[
            ("a.example.com.", "b.example.net."),
            ("b.example.net.", "a.example.com."),
        ]);
        assert!(!chain_leaves_public("a.example.com.", looping).await);
    }
}
