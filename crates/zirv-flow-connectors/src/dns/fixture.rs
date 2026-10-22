//! A resolver that serves a map, for tests: set a name's records, take them away, or make the
//! name servers "fail". Clones share the map, so a test can change DNS under a running job.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use hashbrown::{HashMap, HashSet};

use super::{DnsError, DnsResolver, Lookup, chain_is_public};

/// The longest CNAME chain the fixture follows, as a real resolver stops after a few hops.
const MAX_HOPS: usize = 8;

#[derive(Default)]
struct Zone {
    records: HashMap<String, Vec<String>>,
    cnames: HashMap<String, String>,
    failing: HashSet<String>,
    queried: Vec<String>,
}

/// An in-memory zone.
#[derive(Clone, Default)]
pub struct FixtureResolver {
    zone: Arc<Mutex<Zone>>,
}

fn key(name: &str) -> String {
    name.trim().trim_end_matches('.').to_ascii_lowercase()
}

impl std::fmt::Debug for FixtureResolver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("FixtureResolver")
    }
}

impl FixtureResolver {
    pub fn new() -> Self {
        Self::default()
    }

    fn zone(&self) -> MutexGuard<'_, Zone> {
        self.zone.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Replaces the TXT records at `name`.
    pub fn set(&self, name: &str, records: &[&str]) {
        let key = key(name);
        let mut zone = self.zone();
        zone.failing.remove(&key);
        zone.records.insert(
            key,
            records.iter().map(|record| (*record).to_owned()).collect(),
        );
    }

    /// Makes `name` an alias of `target`: a lookup of `name` follows the chain and answers with
    /// what `target` holds, unless a target in the chain is not a public name.
    pub fn set_cname(&self, name: &str, target: &str) {
        self.zone().cnames.insert(key(name), key(target));
    }

    /// Removes every record at `name`, as if the name had none.
    pub fn remove(&self, name: &str) {
        self.zone().records.remove(&key(name));
    }

    /// Makes lookups of `name` fail as if the name servers did not answer, until it is `set`.
    pub fn fail(&self, name: &str) {
        self.zone().failing.insert(key(name));
    }

    /// The names looked up so far, in order.
    pub fn queried(&self) -> Vec<String> {
        self.zone().queried.clone()
    }
}

impl DnsResolver for FixtureResolver {
    fn lookup_txt<'a>(&'a self, name: &'a str) -> Lookup<'a> {
        Box::pin(async move {
            let key = key(name);
            let mut zone = self.zone();
            zone.queried.push(key.clone());
            let mut name = key;
            let mut targets = Vec::new();
            for _ in 0..MAX_HOPS {
                match zone.cnames.get(&name) {
                    Some(target) => {
                        targets.push(target.clone());
                        name = target.clone();
                    }
                    None => break,
                }
            }
            // As the system resolver does: a chain that leaves the public namespace voids the
            // answer.
            if !chain_is_public(&targets) {
                return Ok(Vec::new());
            }
            if zone.failing.contains(&name) {
                return Err(DnsError::Unavailable);
            }
            Ok(zone.records.get(&name).cloned().unwrap_or_default())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_name_serves_what_was_set_and_nothing_after_remove() {
        let resolver = FixtureResolver::new();
        resolver.set("Zirv._domainkey.Example.com.", &["v=DKIM1; p=AAAA"]);

        assert_eq!(
            resolver
                .lookup_txt("zirv._domainkey.example.com")
                .await
                .unwrap(),
            ["v=DKIM1; p=AAAA"]
        );
        resolver.remove("zirv._domainkey.example.com");
        assert!(
            resolver
                .lookup_txt("zirv._domainkey.example.com")
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            resolver
                .lookup_txt("other.example.com")
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_failing_name_is_unavailable_until_it_is_set_again() {
        let resolver = FixtureResolver::new();
        resolver.fail("example.com");

        assert_eq!(
            resolver.lookup_txt("example.com").await,
            Err(DnsError::Unavailable)
        );
        resolver.set("example.com", &["v=spf1 -all"]);
        assert_eq!(
            resolver.lookup_txt("example.com").await.unwrap(),
            ["v=spf1 -all"]
        );
    }

    #[tokio::test]
    async fn clones_share_the_zone_and_the_query_log() {
        let resolver = FixtureResolver::new();
        let clone = resolver.clone();
        clone.set("a.example.com", &["x"]);

        let _ = resolver.lookup_txt("A.example.com.").await;

        assert_eq!(clone.queried(), ["a.example.com"]);
    }

    #[tokio::test]
    async fn an_alias_chain_that_stays_public_resolves_to_its_target() {
        let resolver = FixtureResolver::new();
        resolver.set_cname("_dmarc.attacker.example.com", "hop.provider.example.net");
        resolver.set_cname("hop.provider.example.net", "dmarc.provider.example.org");
        resolver.set("dmarc.provider.example.org", &["v=DMARC1; p=none"]);

        let found = resolver
            .lookup_txt("_dmarc.attacker.example.com")
            .await
            .unwrap();

        assert_eq!(found, ["v=DMARC1; p=none"]);
    }

    #[tokio::test]
    async fn an_alias_chain_into_a_private_zone_is_refused_and_returns_no_records() {
        let resolver = FixtureResolver::new();
        resolver.set_cname("_dmarc.attacker.example.com", "hop.provider.example.net");
        resolver.set_cname("hop.provider.example.net", "secret.corp");
        resolver.set("secret.corp", &["internal-token=hunter2"]);
        resolver.set_cname("direct.example.com", "x.internal");
        resolver.set("x.internal", &["v=spf1 -all"]);

        for name in ["_dmarc.attacker.example.com", "direct.example.com"] {
            assert!(
                resolver.lookup_txt(name).await.unwrap().is_empty(),
                "{name}"
            );
        }
    }
}
