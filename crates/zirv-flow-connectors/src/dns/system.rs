//! The real resolver: `hickory-resolver` on the operating system's name servers.

use std::time::Duration;

use hickory_resolver::TokioResolver;
use hickory_resolver::proto::rr::RData;

use super::{DnsError, DnsResolver, Lookup};

/// How long one name server gets to answer, and how many tries a lookup makes. A verification
/// run makes three lookups, so a dead server delays it by well under a minute.
const TIMEOUT: Duration = Duration::from_secs(5);
const ATTEMPTS: usize = 2;
/// The most TXT records read from one answer. A name with more is not a sending domain's.
const MAX_RECORDS: usize = 64;

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
        let resolver = builder.build().map_err(|_| DnsError::Unavailable)?;
        Ok(Self { resolver })
    }
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
            match self.resolver.txt_lookup(absolute.as_str()).await {
                Ok(lookup) => Ok(lookup
                    .answers()
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
                    .collect()),
                Err(error) if error.is_no_records_found() => Ok(Vec::new()),
                Err(_) => Err(DnsError::Unavailable),
            }
        })
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
}
