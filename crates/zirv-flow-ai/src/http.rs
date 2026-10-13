//! The guarded HTTP client for model calls: the egress policy applied to every connection.
//!
//! Names are resolved through a hook that checks every address with
//! [`EgressPolicy::decide_all`] and hands the connector only the checked ones, so DNS cannot change
//! between the check and the connection. IP-literal hosts never reach a resolver and are decided
//! directly. Redirects are never followed (provider APIs do not redirect) and proxies from the
//! environment are ignored, because a proxy would see the request before the policy did.
//!
//! This crate may not depend on `zirv-flow-connectors` (architecture 5.1), so it carries this
//! small client; both delegate every decision to `zirv_flow_core::egress`.

use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::header::{CONTENT_TYPE, HeaderName, HeaderValue};
use reqwest::redirect::Policy;
use url::{Host, Url};
use zirv_flow_core::SecretString;
use zirv_flow_core::egress::{AddressClass, Decision, EgressPolicy};

use crate::error::{AiError, Denied};

/// The default cap on a provider's response body: 4 MiB.
pub const DEFAULT_MAX_BODY_BYTES: u64 = 4 * 1024 * 1024;

/// How long and how large a model call may be.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AiHttpLimits {
    pub connect_timeout: Duration,
    /// The whole exchange, response body included.
    pub timeout: Duration,
    pub max_body_bytes: u64,
}

impl Default for AiHttpLimits {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            timeout: Duration::from_secs(60),
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
        }
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

struct SystemLookup;

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
fn record_denial(host: &str, class: AddressClass) {
    metrics::counter!("zirv_flow_egress_denied_total", "class" => class.as_str()).increment(1);
    tracing::warn!(
        host,
        class = class.as_str(),
        "outbound connection refused by the egress policy"
    );
}

/// Checks a resolved set against the policy, recording a refusal: one refused address refuses
/// them all.
fn check_all(policy: &EgressPolicy, host: &str, ips: &[IpAddr]) -> Result<(), Denied> {
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
struct GuardedResolver {
    policy: Arc<EgressPolicy>,
    lookup: Arc<dyn Lookup>,
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

/// How a request authenticates to the provider.
pub(crate) enum Auth<'a> {
    /// `Authorization: Bearer <key>`.
    Bearer(&'a SecretString),
    /// A named header carrying the key, such as `x-api-key`.
    Header(&'static str, &'a SecretString),
}

/// The client every provider adapter calls through.
#[derive(Clone)]
pub struct AiHttp {
    inner: reqwest::Client,
    policy: Arc<EgressPolicy>,
    limits: AiHttpLimits,
}

impl std::fmt::Debug for AiHttp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AiHttp")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl AiHttp {
    pub fn new(policy: Arc<EgressPolicy>, limits: AiHttpLimits) -> Result<Self, AiError> {
        Self::with_lookup(policy, limits, Arc::new(SystemLookup))
    }

    /// As [`AiHttp::new`], resolving names through `lookup`. The policy still applies to whatever
    /// `lookup` returns.
    pub fn with_lookup(
        policy: Arc<EgressPolicy>,
        limits: AiHttpLimits,
        lookup: Arc<dyn Lookup>,
    ) -> Result<Self, AiError> {
        // reqwest takes the process-wide rustls provider; another install is fine.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let inner = reqwest::Client::builder()
            .dns_resolver(Arc::new(GuardedResolver {
                policy: Arc::clone(&policy),
                lookup,
            }))
            .no_proxy()
            .redirect(Policy::none())
            .connect_timeout(limits.connect_timeout)
            .timeout(limits.timeout)
            .build()
            .map_err(|_| AiError::Transport("the HTTP client could not be built".to_owned()))?;
        Ok(Self {
            inner,
            policy,
            limits,
        })
    }

    pub fn limits(&self) -> AiHttpLimits {
        self.limits
    }

    /// Posts a JSON body and returns the response body of a success. A non-success status is
    /// [`AiError::Http`] and its body is dropped unread: provider error text can echo the prompt.
    pub(crate) async fn post_json(
        &self,
        url: &Url,
        auth: Auth<'_>,
        extra: &[(&'static str, &'static str)],
        body: Vec<u8>,
    ) -> Result<Vec<u8>, AiError> {
        check_literal(&self.policy, url.host())?;
        let mut request = self
            .inner
            .post(url.clone())
            .header(CONTENT_TYPE, "application/json")
            .body(body);
        for (name, value) in extra {
            request = request.header(*name, *value);
        }
        let (name, value) = match auth {
            Auth::Bearer(key) => (
                HeaderName::from_static("authorization"),
                format!("Bearer {}", key.expose()),
            ),
            Auth::Header(name, key) => (HeaderName::from_static(name), key.expose().to_owned()),
        };
        let mut value = HeaderValue::from_str(&value)
            .map_err(|_| AiError::Transport("the provider key is not a valid header".to_owned()))?;
        value.set_sensitive(true);
        let response = request
            .header(name, value)
            .send()
            .await
            .map_err(AiError::from_reqwest)?;
        let status = response.status();
        if !status.is_success() {
            return Err(AiError::Http(status.as_u16()));
        }
        read_body(response, self.limits.max_body_bytes).await
    }
}

/// Builds the URL of a provider call: the base URL with `path` appended. The base must be an
/// http(s) URL with a host and no query, fragment or credentials.
pub(crate) fn endpoint_url(base: &str, path: &str) -> Result<Url, AiError> {
    let mut url = Url::parse(base).map_err(|_| AiError::InvalidEndpoint)?;
    let valid = matches!(url.scheme(), "http" | "https")
        && url.host().is_some()
        && url.query().is_none()
        && url.fragment().is_none()
        && url.username().is_empty()
        && url.password().is_none();
    if !valid {
        return Err(AiError::InvalidEndpoint);
    }
    let joined = format!("{}{path}", url.path().trim_end_matches('/'));
    url.set_path(&joined);
    Ok(url)
}

/// Decides an IP-literal host; a name passes, because the resolver decides it when connecting.
fn check_literal(policy: &EgressPolicy, host: Option<Host<&str>>) -> Result<(), AiError> {
    let ip = match host {
        Some(Host::Ipv4(ip)) => IpAddr::V4(ip),
        Some(Host::Ipv6(ip)) => IpAddr::V6(ip),
        Some(Host::Domain(_)) | None => return Ok(()),
    };
    check_all(policy, &ip.to_string(), &[ip]).map_err(|denied| AiError::Egress {
        host: denied.host,
        class: denied.class,
    })
}

/// Reads the whole body, failing as soon as it exceeds `max_bytes`. A declared `Content-Length`
/// over the cap fails before any byte is read; a body without one is cut off while streaming.
async fn read_body(mut response: reqwest::Response, max_bytes: u64) -> Result<Vec<u8>, AiError> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes)
    {
        return Err(AiError::Malformed);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(AiError::from_reqwest)? {
        if (body.len() as u64).saturating_add(chunk.len() as u64) > max_bytes {
            return Err(AiError::Malformed);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use hashbrown::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Router;
    use axum::extract::State;
    use axum::http::HeaderMap;
    use axum::response::Redirect;
    use axum::routing::post;

    use super::*;

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
        ])))
    }

    fn http(allow: &[&str], limits: AiHttpLimits) -> AiHttp {
        let allow: Vec<String> = allow.iter().map(|entry| (*entry).to_owned()).collect();
        let policy = Arc::new(EgressPolicy::parse(&allow).unwrap());
        AiHttp::with_lookup(policy, limits, names()).unwrap()
    }

    #[derive(Default)]
    struct Seen {
        hits: AtomicUsize,
        auth: Mutex<Vec<String>>,
    }

    async fn serve() -> (u16, Arc<Seen>) {
        let seen = Arc::new(Seen::default());
        let router = Router::new()
            .route(
                "/ok",
                post(
                    |State(seen): State<Arc<Seen>>, headers: HeaderMap, body: String| async move {
                        seen.hits.fetch_add(1, Ordering::SeqCst);
                        let auth = headers
                            .get("authorization")
                            .or_else(|| headers.get("x-api-key"))
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or_default()
                            .to_owned();
                        seen.auth.lock().unwrap().push(auth);
                        format!("echo:{body}")
                    },
                ),
            )
            .route(
                "/redirect",
                post(|State(seen): State<Arc<Seen>>| async move {
                    seen.hits.fetch_add(1, Ordering::SeqCst);
                    Redirect::temporary("http://169.254.169.254/latest")
                }),
            )
            .route(
                "/fail",
                post(|| async { (axum::http::StatusCode::BAD_GATEWAY, "PROMPT-ECHO") }),
            )
            .route("/big", post(|| async { "x".repeat(1000) }))
            .route(
                "/slow",
                post(|| async {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    "late"
                }),
            )
            .with_state(Arc::clone(&seen));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (port, seen)
    }

    async fn post_to(http: &AiHttp, url: &str, key: &str) -> Result<Vec<u8>, AiError> {
        let key = SecretString::new(key.to_owned());
        http.post_json(
            &Url::parse(url).unwrap(),
            Auth::Bearer(&key),
            &[],
            b"{}".to_vec(),
        )
        .await
    }

    #[tokio::test]
    async fn a_host_resolving_to_loopback_is_blocked_until_its_cidr_is_allowlisted() {
        let (port, seen) = serve().await;
        let url = format!("http://local.test:{port}/ok");

        let blocked = post_to(&http(&[], AiHttpLimits::default()), &url, "k").await;
        assert_eq!(
            blocked.unwrap_err(),
            AiError::Egress {
                host: "local.test".into(),
                class: AddressClass::Loopback
            }
        );
        assert_eq!(seen.hits.load(Ordering::SeqCst), 0);

        let allowed = http(&["127.0.0.1/32"], AiHttpLimits::default());
        assert_eq!(post_to(&allowed, &url, "k").await.unwrap(), b"echo:{}");
        assert_eq!(seen.hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn ip_literals_are_decided_before_sending() {
        let (port, seen) = serve().await;
        let url = format!("http://127.0.0.1:{port}/ok");

        let blocked = post_to(&http(&[], AiHttpLimits::default()), &url, "k").await;
        assert!(matches!(
            blocked,
            Err(AiError::Egress {
                class: AddressClass::Loopback,
                ..
            })
        ));
        let meta = post_to(
            &http(&["127.0.0.1/32"], AiHttpLimits::default()),
            "http://169.254.169.254/",
            "k",
        )
        .await;
        assert!(matches!(
            meta,
            Err(AiError::Egress {
                class: AddressClass::Metadata,
                ..
            })
        ));
        assert_eq!(seen.hits.load(Ordering::SeqCst), 0);

        let allowed = http(&["127.0.0.1/32"], AiHttpLimits::default());
        assert!(post_to(&allowed, &url, "k").await.is_ok());
    }

    #[tokio::test]
    async fn a_private_or_mixed_resolution_stays_blocked_when_only_loopback_is_allowlisted() {
        let guarded = http(&["127.0.0.1/32"], AiHttpLimits::default());
        let private = post_to(&guarded, "http://private.test:9/ok", "k").await;
        assert_eq!(
            private.unwrap_err(),
            AiError::Egress {
                host: "private.test".into(),
                class: AddressClass::Private
            }
        );
        // One denied address refuses the whole set, even beside a public one.
        let unlisted = http(&[], AiHttpLimits::default());
        let mixed = post_to(&unlisted, "http://mixed.test:9/ok", "k").await;
        assert_eq!(
            mixed.unwrap_err(),
            AiError::Egress {
                host: "mixed.test".into(),
                class: AddressClass::Loopback
            }
        );
    }

    #[tokio::test]
    async fn a_redirect_is_never_followed() {
        let (port, seen) = serve().await;
        let guarded = http(&["127.0.0.1/32"], AiHttpLimits::default());
        let result = post_to(&guarded, &format!("http://127.0.0.1:{port}/redirect"), "k").await;
        assert_eq!(result.unwrap_err(), AiError::Http(307));
        assert_eq!(seen.hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_failure_status_drops_the_body_and_names_only_the_status() {
        let (port, _) = serve().await;
        let guarded = http(&["127.0.0.1/32"], AiHttpLimits::default());
        let error = post_to(&guarded, &format!("http://127.0.0.1:{port}/fail"), "k")
            .await
            .unwrap_err();
        assert_eq!(error, AiError::Http(502));
        assert!(!format!("{error} {error:?}").contains("PROMPT-ECHO"));
    }

    #[tokio::test]
    async fn the_body_is_capped() {
        let (port, _) = serve().await;
        let limits = AiHttpLimits {
            max_body_bytes: 100,
            ..AiHttpLimits::default()
        };
        let guarded = http(&["127.0.0.1/32"], limits);
        let result = post_to(&guarded, &format!("http://127.0.0.1:{port}/big"), "k").await;
        assert_eq!(result.unwrap_err(), AiError::Malformed);
    }

    #[tokio::test]
    async fn a_slow_provider_times_out() {
        let (port, _) = serve().await;
        let limits = AiHttpLimits {
            timeout: Duration::from_millis(200),
            ..AiHttpLimits::default()
        };
        let guarded = http(&["127.0.0.1/32"], limits);
        let result = post_to(&guarded, &format!("http://127.0.0.1:{port}/slow"), "k").await;
        assert_eq!(result.unwrap_err(), AiError::Timeout);
    }

    #[tokio::test]
    async fn the_key_is_sent_as_a_bearer_or_a_named_header() {
        let (port, seen) = serve().await;
        let guarded = http(&["127.0.0.1/32"], AiHttpLimits::default());
        let url = Url::parse(&format!("http://127.0.0.1:{port}/ok")).unwrap();
        let key = SecretString::new("sk-test".to_owned());

        guarded
            .post_json(&url, Auth::Bearer(&key), &[], b"{}".to_vec())
            .await
            .unwrap();
        guarded
            .post_json(&url, Auth::Header("x-api-key", &key), &[], b"{}".to_vec())
            .await
            .unwrap();

        assert_eq!(
            *seen.auth.lock().unwrap(),
            vec!["Bearer sk-test".to_owned(), "sk-test".to_owned()]
        );
    }

    #[tokio::test]
    async fn environment_proxies_are_ignored() {
        // Safety: this test is the only one in the process that touches the proxy variables.
        // SAFETY: nextest runs each test in its own process.
        unsafe { std::env::set_var("HTTP_PROXY", "http://127.0.0.1:1") };
        let (port, _) = serve().await;
        let guarded = http(&["127.0.0.1/32"], AiHttpLimits::default());
        let result = post_to(&guarded, &format!("http://127.0.0.1:{port}/ok"), "k").await;
        assert!(result.is_ok(), "{result:?}");
    }

    #[tokio::test]
    async fn errors_and_debug_output_carry_no_key_query_or_url() {
        let guarded = http(&["127.0.0.1/32"], AiHttpLimits::default());
        let error = post_to(
            &guarded,
            "http://127.0.0.1:1/secret/path?token=hunter2",
            "sk-live-key",
        )
        .await
        .unwrap_err();
        let text = format!("{error} {error:?} {guarded:?}");
        for leaked in ["hunter2", "sk-live-key", "secret/path", "127.0.0.1:1"] {
            assert!(!text.contains(leaked), "{text}");
        }
    }

    #[test]
    fn endpoint_urls_join_the_base_and_refuse_unusable_bases() {
        let join = |base: &str| endpoint_url(base, "/chat/completions").map(|url| url.to_string());
        assert_eq!(
            join("https://api.example.com/v1").unwrap(),
            "https://api.example.com/v1/chat/completions"
        );
        assert_eq!(
            join("http://localhost:8080/").unwrap(),
            "http://localhost:8080/chat/completions"
        );
        for bad in [
            "ftp://x.test/",
            "not a url",
            "https://u:p@x.test/",
            "https://x.test/?key=1",
            "https://x.test/#f",
        ] {
            assert_eq!(join(bad).unwrap_err(), AiError::InvalidEndpoint, "{bad}");
        }
    }

    #[test]
    fn the_defaults_are_ten_and_sixty_seconds_and_four_mebibytes() {
        let limits = AiHttpLimits::default();
        assert_eq!(limits.connect_timeout, Duration::from_secs(10));
        assert_eq!(limits.timeout, Duration::from_secs(60));
        assert_eq!(limits.max_body_bytes, 4 * 1024 * 1024);
    }

    #[test]
    fn no_source_file_builds_a_client_outside_the_guard() {
        let sources = [
            ("lib.rs", include_str!("lib.rs")),
            ("http.rs", include_str!("http.rs")),
            ("error.rs", include_str!("error.rs")),
            ("schema.rs", include_str!("schema.rs")),
            ("request.rs", include_str!("request.rs")),
            ("response.rs", include_str!("response.rs")),
            ("provider.rs", include_str!("provider.rs")),
            ("probe.rs", include_str!("probe.rs")),
            ("repair.rs", include_str!("repair.rs")),
            ("fake.rs", include_str!("fake.rs")),
            ("providers/mod.rs", include_str!("providers/mod.rs")),
            ("providers/openai.rs", include_str!("providers/openai.rs")),
            (
                "providers/anthropic.rs",
                include_str!("providers/anthropic.rs"),
            ),
        ];
        let mut builders = 0;
        for (name, source) in sources {
            let production = source.split("#[cfg(test)]").next().unwrap();
            assert!(!production.contains("Client::new("), "{name}");
            assert!(!production.contains("pub fn client"), "{name}");
            assert!(!production.contains("redirect(Policy::limited"), "{name}");
            builders += production.matches("Client::builder(").count();
        }
        assert_eq!(
            builders, 1,
            "exactly one client is built, and it is the guarded one"
        );
    }
}
