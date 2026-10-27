//! The DNS hook that is the connect point of the egress guard. reqwest dials exactly the
//! addresses this resolver returns, so the addresses that were checked are the addresses used.

use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use zirv_flow_core::egress::{AddressClass, Decision, EgressPolicy};

use super::error::Denied;

/// Normalize URL-style numeric hosts as well as bare IPv6 before consulting DNS. A host is
/// never a URL, userinfo, or a scoped address; do not put rejected input in errors or logs.
pub(crate) fn parse_host(host: &str) -> Result<url::Host<String>, Denied> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(match ip {
            IpAddr::V4(ip) => url::Host::Ipv4(ip),
            IpAddr::V6(ip) => url::Host::Ipv6(ip),
        });
    }
    url::Host::parse(host).map_err(|_| invalid_host())
}

pub(crate) fn invalid_host() -> Denied {
    record_denial("<invalid host>", AddressClass::Reserved);
    Denied {
        host: "<invalid host>".to_owned(),
        class: AddressClass::Reserved,
    }
}

/// Resolves a host name to addresses. Production uses the system resolver; tests inject their own
/// to make a name resolve to chosen addresses without touching DNS.
pub trait Lookup: Send + Sync {
    fn lookup<'a>(
        &'a self,
        host: &'a str,
    ) -> Pin<Box<dyn Future<Output = io::Result<Vec<IpAddr>>> + Send + 'a>>;
}

/// The system resolver, through tokio.
pub(crate) struct SystemLookup;

impl Lookup for SystemLookup {
    fn lookup<'a>(
        &'a self,
        host: &'a str,
    ) -> Pin<Box<dyn Future<Output = io::Result<Vec<IpAddr>>> + Send + 'a>> {
        Box::pin(async move {
            let addresses = tokio::net::lookup_host((host, 0)).await?;
            Ok(addresses.map(|address| address.ip()).collect())
        })
    }
}

/// Counts and logs a refusal. The log names the host and class, never the path or query.
pub(crate) fn record_denial(host: &str, class: AddressClass) {
    metrics::counter!("zirv_flow_egress_denied_total", "class" => class.as_str()).increment(1);
    tracing::warn!(
        host,
        class = class.as_str(),
        "outbound connection refused by the egress policy"
    );
}

/// Checks a resolved set against the policy, recording a refusal: one refused address refuses
/// them all.
pub(crate) fn check_all(policy: &EgressPolicy, host: &str, ips: &[IpAddr]) -> Result<(), Denied> {
    match policy.decide_all(ips) {
        Decision::Allow => Ok(()),
        Decision::Deny(class) => {
            record_denial(host, class);
            Err(Denied {
                host: host.to_owned(),
                class,
            })
        }
    }
}

/// A reqwest resolver that refuses any host whose resolution includes a denied address.
pub(crate) struct GuardedResolver {
    policy: Arc<EgressPolicy>,
    lookup: Arc<dyn Lookup>,
}

impl GuardedResolver {
    pub(crate) fn new(policy: Arc<EgressPolicy>, lookup: Arc<dyn Lookup>) -> Self {
        Self { policy, lookup }
    }
}

impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let policy = Arc::clone(&self.policy);
        let lookup = Arc::clone(&self.lookup);
        Box::pin(async move {
            let host = name.as_str();
            let ips = lookup.lookup(host).await?;
            if ips.is_empty() {
                return Err(io::Error::new(io::ErrorKind::NotFound, "no addresses").into());
            }
            check_all(&policy, host, &ips)?;
            // Port 0: reqwest substitutes the URL's port.
            let addrs: Addrs = Box::new(ips.into_iter().map(|ip| SocketAddr::new(ip, 0)));
            Ok(addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed(Vec<IpAddr>);

    impl Lookup for Fixed {
        fn lookup<'a>(
            &'a self,
            _host: &'a str,
        ) -> Pin<Box<dyn Future<Output = io::Result<Vec<IpAddr>>> + Send + 'a>> {
            Box::pin(async move { Ok(self.0.clone()) })
        }
    }

    fn resolver(entries: &[&str], ips: &[&str]) -> GuardedResolver {
        let entries: Vec<String> = entries.iter().map(|e| (*e).to_owned()).collect();
        let policy = Arc::new(EgressPolicy::parse(&entries).unwrap());
        let ips = ips.iter().map(|ip| ip.parse().unwrap()).collect();
        GuardedResolver::new(policy, Arc::new(Fixed(ips)))
    }

    async fn resolve(resolver: &GuardedResolver) -> Result<Vec<SocketAddr>, String> {
        let name: Name = "example.test".parse().unwrap();
        match resolver.resolve(name).await {
            Ok(addrs) => Ok(addrs.collect()),
            Err(error) => match error.downcast::<Denied>() {
                Ok(denied) => Err(format!("{}:{}", denied.host, denied.class)),
                Err(other) => Err(other.to_string()),
            },
        }
    }

    #[tokio::test]
    async fn public_then_private_dns_answers_are_each_checked_once() {
        use crate::http::testing::Rebinding;

        for later in ["10.0.0.5", "169.254.169.254"] {
            let lookup = Arc::new(Rebinding::new("8.8.8.8", later));
            let resolver = GuardedResolver::new(Arc::default(), lookup.clone());
            assert_eq!(
                resolve(&resolver).await.unwrap(),
                vec!["8.8.8.8:0".parse::<SocketAddr>().unwrap()]
            );
            assert_eq!(lookup.calls(), 1);
            assert!(resolve(&resolver).await.is_err());
            assert_eq!(lookup.calls(), 2);
        }
    }

    #[tokio::test]
    async fn public_addresses_are_returned_for_the_connector_to_dial() {
        let addrs = resolve(&resolver(&[], &["8.8.8.8", "2606:4700::1111"]))
            .await
            .unwrap();
        assert_eq!(addrs.len(), 2);
        assert_eq!(addrs[0].ip(), "8.8.8.8".parse::<IpAddr>().unwrap());
    }

    #[tokio::test]
    async fn a_denied_address_refuses_the_host_with_its_class() {
        let error = resolve(&resolver(&[], &["127.0.0.1"])).await.unwrap_err();
        assert_eq!(error, "example.test:loopback");
    }

    #[tokio::test]
    async fn a_mixed_resolution_is_refused_whole() {
        let error = resolve(&resolver(&[], &["8.8.8.8", "10.0.0.1"]))
            .await
            .unwrap_err();
        assert_eq!(error, "example.test:private");
    }

    #[tokio::test]
    async fn an_allowlisted_address_is_returned() {
        let addrs = resolve(&resolver(&["127.0.0.1/32"], &["127.0.0.1"]))
            .await
            .unwrap();
        assert_eq!(addrs, vec!["127.0.0.1:0".parse().unwrap()]);
    }

    #[tokio::test]
    async fn an_empty_resolution_is_an_error() {
        let error = resolve(&resolver(&[], &[])).await.unwrap_err();
        assert!(error.contains("no addresses"), "{error}");
    }

    #[tokio::test]
    async fn the_system_lookup_resolves_an_ip_literal_without_dns() {
        let ips = SystemLookup.lookup("127.0.0.1").await.unwrap();
        assert!(ips.contains(&"127.0.0.1".parse::<IpAddr>().unwrap()));
    }

    #[test]
    fn a_denial_is_counted_by_class() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            let policy = EgressPolicy::default();
            assert!(check_all(&policy, "a.test", &["10.0.0.1".parse().unwrap()]).is_err());
            assert!(check_all(&policy, "b.test", &["8.8.8.8".parse().unwrap()]).is_ok());
        });
        let rendered = handle.render();
        assert!(
            rendered.contains("zirv_flow_egress_denied_total{class=\"private\"} 1"),
            "{rendered}"
        );
    }
}
