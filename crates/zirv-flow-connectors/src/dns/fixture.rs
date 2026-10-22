//! A resolver that serves a map, for tests: set a name's records, take them away, or make the
//! name servers "fail". Clones share the map, so a test can change DNS under a running job.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use hashbrown::{HashMap, HashSet};

use super::{DnsError, DnsResolver, Lookup};

#[derive(Default)]
struct Zone {
    records: HashMap<String, Vec<String>>,
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
            if zone.failing.contains(&key) {
                return Err(DnsError::Unavailable);
            }
            Ok(zone.records.get(&key).cloned().unwrap_or_default())
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
}
