//! The guarded client: the egress policy applied to every request and every redirect hop.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use reqwest::redirect::{Attempt, Policy};
use url::Host;
use zirv_flow_core::egress::EgressPolicy;

use super::body::DEFAULT_MAX_BODY_BYTES;
use super::error::{Denied, HttpError, TooManyRedirects};
use super::resolver::{GuardedResolver, Lookup, SystemLookup, check_all};

/// How long and how large an outbound exchange may be.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HttpLimits {
    /// The most redirect hops followed; each is checked against the policy again.
    pub max_redirects: usize,
    pub connect_timeout: Duration,
    /// The whole exchange, response body included.
    pub timeout: Duration,
    /// The cap callers pass to [`read_body`](super::read_body).
    pub max_body_bytes: u64,
}

impl Default for HttpLimits {
    fn default() -> Self {
        Self {
            max_redirects: 5,
            connect_timeout: Duration::from_secs(10),
            timeout: Duration::from_secs(30),
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
        }
    }
}

/// An HTTP client that refuses destinations the egress policy denies.
///
/// Names are resolved through [`GuardedResolver`], which checks every address and hands the
/// connector only the checked ones, so DNS cannot change between the check and the connection.
/// IP-literal hosts never reach a resolver and are decided directly, before sending and on every
/// redirect. Proxies from the environment are ignored, because a proxy would see the request
/// before the policy did.
#[derive(Clone)]
pub struct GuardedClient {
    inner: reqwest::Client,
    policy: Arc<EgressPolicy>,
    limits: HttpLimits,
}

impl GuardedClient {
    pub fn new(policy: Arc<EgressPolicy>, limits: HttpLimits) -> Result<Self, HttpError> {
        Self::with_lookup(policy, limits, Arc::new(SystemLookup))
    }

    /// As [`GuardedClient::new`], resolving names through `lookup`. The policy still applies to
    /// whatever `lookup` returns.
    pub fn with_lookup(
        policy: Arc<EgressPolicy>,
        limits: HttpLimits,
        lookup: Arc<dyn Lookup>,
    ) -> Result<Self, HttpError> {
        // reqwest takes the process-wide rustls provider; another install is fine.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let inner = reqwest::Client::builder()
            .dns_resolver(Arc::new(GuardedResolver::new(Arc::clone(&policy), lookup)))
            .no_proxy()
            .redirect(redirect_policy(Arc::clone(&policy), limits.max_redirects))
            .connect_timeout(limits.connect_timeout)
            .timeout(limits.timeout)
            .build()
            .map_err(|_| HttpError::Transport("the HTTP client could not be built".to_owned()))?;
        Ok(Self {
            inner,
            policy,
            limits,
        })
    }

    pub fn limits(&self) -> HttpLimits {
        self.limits
    }

    /// Sends the request. An IP-literal host is decided here; any other host is decided by the
    /// resolver when the connection is made.
    pub async fn execute(&self, request: reqwest::Request) -> Result<reqwest::Response, HttpError> {
        check_literal(&self.policy, request.url().host())?;
        self.inner
            .execute(request)
            .await
            .map_err(HttpError::from_reqwest)
    }
}

/// Decides an IP-literal host; a name passes, because the resolver decides it when connecting.
fn check_literal(policy: &EgressPolicy, host: Option<Host<&str>>) -> Result<(), Denied> {
    let ip = match host {
        Some(Host::Ipv4(ip)) => IpAddr::V4(ip),
        Some(Host::Ipv6(ip)) => IpAddr::V6(ip),
        Some(Host::Domain(_)) | None => return Ok(()),
    };
    check_all(policy, &ip.to_string(), &[ip])
}

fn redirect_policy(policy: Arc<EgressPolicy>, max_redirects: usize) -> Policy {
    Policy::custom(move |attempt: Attempt| {
        // `previous` holds the original URL and every hop already followed.
        if attempt.previous().len() > max_redirects {
            return attempt.error(TooManyRedirects);
        }
        // A name is resolved, and so checked, when the next hop connects.
        match check_literal(&policy, attempt.url().host()) {
            Ok(()) => attempt.follow(),
            Err(denied) => attempt.error(denied),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::future::Future;
    use std::io;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Router;
    use axum::body::{Body, Bytes};
    use axum::extract::State;
    use axum::response::Redirect;
    use axum::routing::get;
    use futures_util::stream;
    use reqwest::Method;
    use zirv_flow_core::egress::AddressClass;

    use crate::http::read_body;

    struct Names(HashMap<&'static str, Vec<IpAddr>>);

    impl Lookup for Names {
        fn lookup<'a>(
            &'a self,
            host: &'a str,
        ) -> Pin<Box<dyn Future<Output = io::Result<Vec<IpAddr>>> + Send + 'a>> {
            Box::pin(async move {
                self.0
                    .get(host)
                    .cloned()
                    .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown test host"))
            })
        }
    }

    fn names() -> Arc<Names> {
        let ips = |list: &[&str]| list.iter().map(|ip| ip.parse().unwrap()).collect();
        Arc::new(Names(HashMap::from([
            ("local.test", ips(&["127.0.0.1"])),
            ("private.test", ips(&["10.0.0.1"])),
            ("mixed.test", ips(&["8.8.8.8", "127.0.0.1"])),
            ("meta.test", ips(&["169.254.169.254"])),
        ])))
    }

    fn client(allow: &[&str], limits: HttpLimits) -> GuardedClient {
        let allow: Vec<String> = allow.iter().map(|entry| (*entry).to_owned()).collect();
        let policy = Arc::new(EgressPolicy::parse(&allow).unwrap());
        GuardedClient::with_lookup(policy, limits, names()).unwrap()
    }

    async fn get_url(client: &GuardedClient, url: &str) -> Result<reqwest::Response, HttpError> {
        client
            .execute(reqwest::Request::new(Method::GET, url.parse().unwrap()))
            .await
    }

    fn denied(host: &str, class: AddressClass) -> HttpError {
        HttpError::EgressDenied {
            host: host.to_owned(),
            class,
        }
    }

    /// A local server; returns its port and a count of the requests it has served.
    async fn serve() -> (u16, Arc<AtomicUsize>) {
        let hits = Arc::new(AtomicUsize::new(0));
        let router =
            Router::new()
                .route("/", get(|| async { "hello" }))
                .route(
                    "/meta",
                    get(|| async {
                        Redirect::temporary("http://169.254.169.254/latest/meta-data")
                    }),
                )
                .route(
                    "/meta-v6",
                    get(|| async { Redirect::temporary("http://[::ffff:169.254.169.254]/") }),
                )
                .route(
                    "/private",
                    get(|| async { Redirect::temporary("http://private.test/") }),
                )
                .route("/loop", get(|| async { Redirect::temporary("/loop") }))
                .route(
                    "/slow",
                    get(|| async {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        "late"
                    }),
                )
                .route("/big", get(|| async { "x".repeat(1000) }))
                .route(
                    "/stream",
                    get(|| async {
                        let chunks =
                            (0..100).map(|_| Ok::<_, io::Error>(Bytes::from(vec![b'x'; 10])));
                        Body::from_stream(stream::iter(chunks))
                    }),
                )
                .layer(axum::middleware::from_fn_with_state(
                    Arc::clone(&hits),
                    |State(hits): State<Arc<AtomicUsize>>,
                     request,
                     next: axum::middleware::Next| async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        next.run(request).await
                    },
                ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (port, hits)
    }

    #[tokio::test]
    async fn a_host_resolving_to_loopback_is_blocked_by_default_and_never_contacted() {
        let (port, hits) = serve().await;
        let result = get_url(
            &client(&[], HttpLimits::default()),
            &format!("http://local.test:{port}/"),
        )
        .await;
        assert_eq!(
            result.unwrap_err(),
            denied("local.test", AddressClass::Loopback)
        );
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn an_allowlisted_address_passes_for_a_name_and_for_a_literal() {
        let (port, hits) = serve().await;
        let guarded = client(&["127.0.0.1/32"], HttpLimits::default());
        for url in [
            format!("http://local.test:{port}/"),
            format!("http://127.0.0.1:{port}/"),
        ] {
            let response = get_url(&guarded, &url).await.unwrap();
            assert_eq!(response.status(), 200);
            assert_eq!(read_body(response, 100).await.unwrap(), b"hello");
        }
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_private_name_stays_blocked_when_only_loopback_is_allowlisted() {
        let guarded = client(&["127.0.0.1/32"], HttpLimits::default());
        let result = get_url(&guarded, "http://private.test:9/").await;
        assert_eq!(
            result.unwrap_err(),
            denied("private.test", AddressClass::Private)
        );
    }

    #[tokio::test]
    async fn a_mixed_public_and_loopback_resolution_is_refused_not_filtered() {
        let (port, hits) = serve().await;
        // The public 8.8.8.8 member does not rescue the set: the whole host is refused, and
        // nothing is dialled.
        let result = get_url(
            &client(&[], HttpLimits::default()),
            &format!("http://mixed.test:{port}/"),
        )
        .await;
        assert_eq!(
            result.unwrap_err(),
            denied("mixed.test", AddressClass::Loopback)
        );
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn ip_literal_urls_are_decided_without_dns() {
        let (port, hits) = serve().await;
        let guarded = client(&[], HttpLimits::default());
        for (url, host, class) in [
            (
                format!("http://127.0.0.1:{port}/"),
                "127.0.0.1",
                AddressClass::Loopback,
            ),
            (
                format!("http://[::1]:{port}/"),
                "::1",
                AddressClass::Loopback,
            ),
            (
                format!("http://[::ffff:127.0.0.1]:{port}/"),
                "::ffff:127.0.0.1",
                AddressClass::Loopback,
            ),
            (
                "http://169.254.169.254/".to_owned(),
                "169.254.169.254",
                AddressClass::Metadata,
            ),
            // The URL parser turns this decimal form into 127.0.0.1.
            (
                format!("http://2130706433:{port}/"),
                "127.0.0.1",
                AddressClass::Loopback,
            ),
        ] {
            let result = get_url(&guarded, &url).await;
            assert_eq!(result.unwrap_err(), denied(host, class), "{url}");
        }
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_redirect_to_a_blocked_literal_is_refused() {
        let (port, _) = serve().await;
        let guarded = client(&["127.0.0.1/32"], HttpLimits::default());
        let result = get_url(&guarded, &format!("http://local.test:{port}/meta")).await;
        assert_eq!(
            result.unwrap_err(),
            denied("169.254.169.254", AddressClass::Metadata)
        );
        let result = get_url(&guarded, &format!("http://local.test:{port}/meta-v6")).await;
        assert_eq!(
            result.unwrap_err(),
            denied("::ffff:169.254.169.254", AddressClass::Metadata)
        );
    }

    #[tokio::test]
    async fn a_redirect_to_a_blocked_hostname_is_refused_at_connect() {
        let (port, _) = serve().await;
        let guarded = client(&["127.0.0.1/32"], HttpLimits::default());
        let result = get_url(&guarded, &format!("http://local.test:{port}/private")).await;
        assert_eq!(
            result.unwrap_err(),
            denied("private.test", AddressClass::Private)
        );
    }

    #[tokio::test]
    async fn redirects_stop_at_the_configured_limit() {
        let (port, hits) = serve().await;
        let limits = HttpLimits {
            max_redirects: 2,
            ..HttpLimits::default()
        };
        let guarded = client(&["127.0.0.1/32"], limits);
        let result = get_url(&guarded, &format!("http://local.test:{port}/loop")).await;
        assert_eq!(
            result.unwrap_err(),
            HttpError::Transport("too many redirects".to_owned())
        );
        // The original request and two followed hops.
        assert_eq!(hits.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn proxy_environment_variables_are_not_used() {
        let (port, hits) = serve().await;
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_port = proxy.local_addr().unwrap().port();
        let proxied = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&proxied);
        tokio::spawn(async move {
            while proxy.accept().await.is_ok() {
                counter.fetch_add(1, Ordering::SeqCst);
            }
        });
        // nextest runs each test in its own process, so changing the environment is safe here.
        for key in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"] {
            unsafe { std::env::set_var(key, format!("http://127.0.0.1:{proxy_port}")) };
        }
        let guarded = client(&["127.0.0.1/32"], HttpLimits::default());
        let response = get_url(&guarded, &format!("http://local.test:{port}/"))
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert_eq!(proxied.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn oversized_bodies_stop_at_the_cap_with_and_without_a_length() {
        let (port, _) = serve().await;
        let guarded = client(&["127.0.0.1/32"], HttpLimits::default());
        let sized = get_url(&guarded, &format!("http://local.test:{port}/big"))
            .await
            .unwrap();
        assert_eq!(read_body(sized, 999).await, Err(HttpError::BodyTooLarge));
        let streamed = get_url(&guarded, &format!("http://local.test:{port}/stream"))
            .await
            .unwrap();
        assert_eq!(streamed.content_length(), None);
        assert_eq!(read_body(streamed, 999).await, Err(HttpError::BodyTooLarge));
        let streamed = get_url(&guarded, &format!("http://local.test:{port}/stream"))
            .await
            .unwrap();
        assert_eq!(read_body(streamed, 1000).await.unwrap().len(), 1000);
    }

    #[tokio::test]
    async fn a_slow_server_hits_the_timeout() {
        let (port, _) = serve().await;
        let limits = HttpLimits {
            timeout: Duration::from_millis(300),
            ..HttpLimits::default()
        };
        let guarded = client(&["127.0.0.1/32"], limits);
        let result = get_url(&guarded, &format!("http://local.test:{port}/slow")).await;
        assert_eq!(result.unwrap_err(), HttpError::Timeout);
    }

    #[tokio::test]
    async fn transport_errors_do_not_echo_the_url() {
        // Nothing listens on port 9, and the credential and query must not appear in the error.
        let guarded = client(&["127.0.0.1/32"], HttpLimits::default());
        let error = get_url(&guarded, "http://user:secret@local.test:9/path?token=abc")
            .await
            .unwrap_err();
        let text = format!("{error} {error:?}");
        for leaked in ["secret", "token", "abc", "path"] {
            assert!(!text.contains(leaked), "{text}");
        }
    }

    #[test]
    fn the_defaults_are_five_redirects_ten_and_thirty_seconds_and_ten_mebibytes() {
        let limits = HttpLimits::default();
        assert_eq!(limits.max_redirects, 5);
        assert_eq!(limits.connect_timeout, Duration::from_secs(10));
        assert_eq!(limits.timeout, Duration::from_secs(30));
        assert_eq!(limits.max_body_bytes, 10 * 1024 * 1024);
    }

    #[test]
    fn no_source_file_builds_a_client_outside_the_guard() {
        let sources = [
            ("lib.rs", include_str!("../lib.rs")),
            ("http/mod.rs", include_str!("mod.rs")),
            ("http/body.rs", include_str!("body.rs")),
            ("http/client.rs", include_str!("client.rs")),
            ("http/error.rs", include_str!("error.rs")),
            ("http/resolver.rs", include_str!("resolver.rs")),
        ];
        let mut builders = 0;
        for (name, source) in sources {
            let production = source.split("#[cfg(test)]").next().unwrap();
            assert!(!production.contains("Client::new("), "{name}");
            assert!(!production.contains("pub fn client"), "{name}");
            builders += production.matches("Client::builder(").count();
        }
        assert_eq!(
            builders, 1,
            "exactly one client is built, and it is the guarded one"
        );
    }
}
