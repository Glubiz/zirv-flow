//! The guarded HTTP client: the one way connectors make outbound HTTP requests.

mod body;
mod client;
mod error;
mod resolver;

#[cfg(test)]
pub(crate) mod testing {
    //! Deterministic DNS answers and hostile authority forms for egress regression tests.

    use std::future::Future;
    use std::io;
    use std::net::IpAddr;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::Lookup;
    use zirv_flow_core::egress::AddressClass;

    pub(crate) const HOSTS: &[(&str, AddressClass)] = &[
        ("allowed@169.254.169.254", AddressClass::Reserved),
        ("2130706433", AddressClass::Loopback),
        ("0177.0.0.1", AddressClass::Loopback),
        ("0x7f.1", AddressClass::Loopback),
        ("[fe80::1%25eth0]", AddressClass::Reserved),
        ("fe80::1%eth0", AddressClass::Reserved),
        ("[::ffff:169.254.169.254]", AddressClass::Metadata),
        ("private.test.", AddressClass::Private),
        ("PRIVATE.TEST", AddressClass::Private),
        ("bücher.test", AddressClass::Private),
    ];

    pub(crate) struct PrivateNames;

    impl Lookup for PrivateNames {
        fn lookup<'a>(
            &'a self,
            host: &'a str,
        ) -> Pin<Box<dyn Future<Output = io::Result<Vec<IpAddr>>> + Send + 'a>> {
            Box::pin(async move {
                match host {
                    "private.test" | "private.test." | "xn--bcher-kva.test" => {
                        Ok(vec!["10.0.0.5".parse().unwrap()])
                    }
                    _ => panic!("unexpected DNS lookup: {host}"),
                }
            })
        }
    }

    pub(crate) struct Rebinding {
        first: IpAddr,
        later: IpAddr,
        calls: AtomicUsize,
    }

    impl Rebinding {
        pub(crate) fn new(first: &str, later: &str) -> Self {
            Self {
                first: first.parse().unwrap(),
                later: later.parse().unwrap(),
                calls: AtomicUsize::new(0),
            }
        }

        pub(crate) fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl Lookup for Rebinding {
        fn lookup<'a>(
            &'a self,
            _host: &'a str,
        ) -> Pin<Box<dyn Future<Output = io::Result<Vec<IpAddr>>> + Send + 'a>> {
            Box::pin(async move {
                Ok(vec![if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    self.first
                } else {
                    self.later
                }])
            })
        }
    }
}

pub use body::DEFAULT_MAX_BODY_BYTES;
pub(crate) use client::parse_url;
pub use client::{GuardedClient, GuardedResponse, HttpLimits};
pub use error::HttpError;
pub use resolver::Lookup;

pub(crate) use resolver::{SystemLookup, check_all, parse_host};
