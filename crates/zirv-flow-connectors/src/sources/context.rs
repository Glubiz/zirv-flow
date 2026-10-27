//! The handle a source gets to the network. A source makes no call except through this.

use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use zirv_flow_core::SecretString;
use zirv_flow_core::egress::EgressPolicy;

use super::error::SourceError;
use crate::http::{
    GuardedClient, HttpError, HttpLimits, Lookup, SystemLookup, check_all, parse_host,
};
use crate::runtime::{
    AccessToken, Auth, RateLimit, RateLimits, RequestSpec, RetryPolicy, RuntimeError,
    RuntimeResponse, TokenSlot, TokenStore, send,
};

/// The most addresses [`SourceContext::resolve_checked`] hands to a driver.
pub(crate) const MAX_RESOLVED: usize = 8;
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(10);

/// The guarded HTTP client and the egress policy behind it. A source that speaks HTTP makes its
/// calls with [`SourceContext::send`], which adds auth, retries and the rate limit; one that
/// resolves and connects by IP itself (database and Kafka sources) must check every address
/// against [`SourceContext::egress_policy`] first.
///
/// A context is for one source. Clones share its OAuth2 token slot.
#[derive(Clone)]
pub struct SourceContext {
    http: GuardedClient,
    policy: Arc<EgressPolicy>,
    /// Resolves host names for the sources that connect by address themselves.
    lookup: Arc<dyn Lookup>,
    /// The source type: the label of the runtime's metrics.
    kind: &'static str,
    /// The source's id: the key of its rate-limit bucket.
    source_id: String,
    rate_limits: Arc<RateLimits>,
    rate_limit: Option<RateLimit>,
    retry: RetryPolicy,
    token: Arc<Mutex<TokenSlot>>,
    /// Held while a token is fetched, so concurrent calls make one request between them.
    flight: Arc<tokio::sync::Mutex<()>>,
    /// Serializes authorization-code refreshes across processes, when the caller provides it.
    token_store: Option<Arc<dyn TokenStore>>,
}

impl SourceContext {
    /// Builds the guarded client for `policy`. `kind` is the source type, `source_id` the source
    /// and `rate_limits` the buckets of this process, shared by every context in it.
    pub fn new(
        policy: Arc<EgressPolicy>,
        limits: HttpLimits,
        kind: &'static str,
        source_id: impl Into<String>,
        rate_limits: Arc<RateLimits>,
    ) -> Result<Self, HttpError> {
        let http = GuardedClient::new(Arc::clone(&policy), limits)?;
        Ok(Self::from_client(
            http,
            policy,
            kind,
            source_id,
            rate_limits,
        ))
    }

    /// Wraps a client built elsewhere, for the same `policy`.
    pub fn from_client(
        http: GuardedClient,
        policy: Arc<EgressPolicy>,
        kind: &'static str,
        source_id: impl Into<String>,
        rate_limits: Arc<RateLimits>,
    ) -> Self {
        Self {
            http,
            policy,
            lookup: Arc::new(SystemLookup),
            kind,
            source_id: source_id.into(),
            rate_limits,
            rate_limit: None,
            retry: RetryPolicy::default(),
            token: Arc::default(),
            flight: Arc::default(),
            token_store: None,
        }
    }

    /// Limits the source's requests per process; `None` is unlimited. The spec of the source
    /// type gives it.
    pub fn with_rate_limit(mut self, rate_limit: Option<RateLimit>) -> Self {
        self.rate_limit = rate_limit;
        self
    }

    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Seeds the OAuth2 token slot with the token the caller stored last time, so the first call
    /// does not fetch one. Only a token with a [binding](AccessToken::binding) is taken: one
    /// without could belong to any client, endpoint or scope, so it is ignored and the first call
    /// fetches a token.
    pub fn with_access_token(self, token: AccessToken) -> Self {
        if token.binding().is_some() {
            // A seed is not a refresh: the caller already has it.
            self.slot().current = Some(token);
        }
        self
    }

    /// Hands authorization-code refreshes to `store`, which persists and serializes them. Without
    /// one the runtime refreshes directly and the caller saves what [`take_refreshed_token`]
    /// returns.
    ///
    /// [`take_refreshed_token`]: SourceContext::take_refreshed_token
    pub fn with_token_store(mut self, store: Arc<dyn TokenStore>) -> Self {
        self.token_store = Some(store);
        self
    }

    /// Resolves names through `lookup` in [`SourceContext::resolve_checked`] instead of the
    /// system resolver. For tests; the policy still applies to whatever `lookup` returns.
    pub fn with_lookup(mut self, lookup: Arc<dyn Lookup>) -> Self {
        self.lookup = lookup;
        self
    }

    /// Resolves `host` and checks every address against the egress policy, for the sources that
    /// connect by address themselves (database sources). Only the returned addresses may be
    /// dialled, and the host name stays the TLS name. Call it at every connect, so a changed DNS
    /// answer is checked again.
    ///
    /// An IP literal skips DNS. An empty answer is `Unreachable`. One denied address refuses the
    /// whole host (counted in `zirv_flow_egress_denied_total`), and an answer of more than
    /// [`MAX_RESOLVED`] distinct addresses is refused only after all of them were checked, so the
    /// cut can never hide a denied one.
    pub async fn resolve_checked(&self, host: &str) -> Result<Vec<IpAddr>, SourceError> {
        let parsed = parse_host(host).map_err(|denied| SourceError::Egress(denied.into()))?;
        let host = parsed.to_string();
        let mut ips = match parsed {
            url::Host::Ipv4(ip) => vec![IpAddr::V4(ip)],
            url::Host::Ipv6(ip) => vec![IpAddr::V6(ip)],
            url::Host::Domain(name) => {
                match tokio::time::timeout(RESOLVE_TIMEOUT, self.lookup.lookup(&name)).await {
                    Ok(Ok(ips)) => ips,
                    Ok(Err(_)) | Err(_) => return Err(SourceError::Unreachable),
                }
            }
        };
        let mut seen = Vec::with_capacity(ips.len());
        ips.retain(|ip| {
            let fresh = !seen.contains(ip);
            seen.push(*ip);
            fresh
        });
        if ips.is_empty() {
            return Err(SourceError::Unreachable);
        }
        check_all(&self.policy, &host, &ips)
            .map_err(|denied| SourceError::Egress(denied.into()))?;
        if ips.len() > MAX_RESOLVED {
            return Err(SourceError::Config {
                field: "host".to_owned(),
                reason: format!("resolves to more than {MAX_RESOLVED} addresses"),
            });
        }
        Ok(ips)
    }

    pub fn http(&self) -> &GuardedClient {
        &self.http
    }

    pub fn egress_policy(&self) -> &Arc<EgressPolicy> {
        &self.policy
    }

    /// Sends `request` with `auth`: waits for the source's rate limit, attaches the auth to a
    /// request for the source's base origin (and refuses any other), executes it through the
    /// guarded client and retries what is worth retrying.
    pub async fn send(
        &self,
        auth: &Auth<'_>,
        request: RequestSpec,
    ) -> Result<RuntimeResponse, RuntimeError> {
        send(self, auth, request).await
    }

    /// The OAuth2 token a call fetched since the caller last asked, for the caller to store.
    /// `None` when no call fetched one. `connectors` never writes to the store itself.
    pub fn take_refreshed_token(&self) -> Option<AccessToken> {
        self.slot().refreshed.take()
    }

    pub(crate) fn kind(&self) -> &'static str {
        self.kind
    }

    pub(crate) fn retry_policy(&self) -> RetryPolicy {
        self.retry
    }

    /// The cached token if it can go on a request for the configuration `binding`.
    pub(crate) fn reusable_token(
        &self,
        binding: &str,
        rejected: Option<&AccessToken>,
    ) -> Option<AccessToken> {
        let slot = self.slot();
        let token = slot.current.as_ref()?;
        if !token.is_usable(binding, rejected) {
            return None;
        }
        Some(token.clone())
    }

    /// Waits for the token fetch in progress, if any, and holds the right to start the next.
    pub(crate) async fn token_flight(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.flight.lock().await
    }

    /// Keeps `token` as the current one; `handed_back` also queues it for the caller to store.
    pub(crate) fn store_token(&self, token: AccessToken, handed_back: bool) {
        let mut slot = self.slot();
        slot.current = Some(token.clone());
        if handed_back {
            slot.refreshed = Some(token);
        }
    }

    /// The newest refresh token the slot holds for the configuration `binding`.
    pub(crate) fn held_refresh_token(&self, binding: &str) -> Option<Arc<SecretString>> {
        let slot = self.slot();
        let token = slot
            .current
            .as_ref()
            .filter(|token| token.binding() == Some(binding))?;
        token.shared_refresh_token()
    }

    pub(crate) fn token_store(&self) -> Option<&Arc<dyn TokenStore>> {
        self.token_store.as_ref()
    }

    fn slot(&self) -> std::sync::MutexGuard<'_, TokenSlot> {
        self.token
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Waits for a token of the source's bucket, and meters the wait.
    pub(crate) async fn wait_for_rate_limit(&self) {
        let Some(rate) = self.rate_limit else {
            return;
        };
        let waited = self.rate_limits.acquire(&self.source_id, rate).await;
        if !waited.is_zero() {
            metrics::gauge!(
                "zirv_flow_source_rate_limit_wait_seconds_total",
                "source_type" => self.kind
            )
            .increment(waited.as_secs_f64());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_context_carries_the_policy_it_was_built_from() {
        let policy = Arc::new(EgressPolicy::default());

        let context = SourceContext::new(
            Arc::clone(&policy),
            HttpLimits::default(),
            "fake",
            "source-1",
            Arc::default(),
        )
        .unwrap();

        assert!(Arc::ptr_eq(context.egress_policy(), &policy));
        assert_eq!(context.http().limits(), HttpLimits::default());
    }

    use crate::sources::sql::testing::{context, fixed};

    use std::future::Future;
    use std::io;
    use std::pin::Pin;

    struct Never;

    #[tokio::test]
    async fn exotic_hosts_are_refused_and_counted_without_echoing_input() {
        use crate::http::testing::{HOSTS, PrivateNames};

        for &(host, class) in HOSTS {
            let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
            let handle = recorder.handle();
            let _guard = metrics::set_default_local_recorder(&recorder);
            let ctx = context("postgres", &[], Arc::new(PrivateNames));
            let error = ctx.resolve_checked(host).await.unwrap_err();
            assert!(
                matches!(error, SourceError::Egress(HttpError::EgressDenied { class: actual, .. }) if actual == class),
                "{host}: {error:?}"
            );
            assert!(!error.to_string().contains("allowed@"));
            assert!(
                handle.render().contains(&format!(
                    "zirv_flow_egress_denied_total{{class=\"{class}\"}} 1"
                )),
                "{host}"
            );
        }
    }

    #[tokio::test]
    async fn public_then_private_dns_is_rechecked_and_an_exact_allowlist_does_not_expand() {
        use crate::http::testing::Rebinding;

        for later in ["10.0.0.5", "169.254.169.254"] {
            let lookup = Arc::new(Rebinding::new("8.8.8.8", later));
            let ctx = context("postgres", &[], lookup.clone());
            assert_eq!(
                ctx.resolve_checked("db.test").await.unwrap(),
                vec!["8.8.8.8".parse::<IpAddr>().unwrap()]
            );
            assert!(matches!(
                ctx.resolve_checked("db.test").await,
                Err(SourceError::Egress(_))
            ));
            assert_eq!(lookup.calls(), 2);
        }
        for (answer, allowed) in [
            ("10.0.0.5", true),
            ("10.0.0.6", false),
            ("169.254.169.254", false),
        ] {
            let ctx = context("postgres", &["10.0.0.5/32"], fixed(&[answer]));
            assert_eq!(
                ctx.resolve_checked("db.test").await.is_ok(),
                allowed,
                "{answer}"
            );
        }
    }

    impl Lookup for Never {
        fn lookup<'a>(
            &'a self,
            _host: &'a str,
        ) -> Pin<Box<dyn Future<Output = io::Result<Vec<IpAddr>>> + Send + 'a>> {
            Box::pin(async move { panic!("an IP literal must not be resolved") })
        }
    }

    #[tokio::test]
    async fn a_public_answer_is_returned_for_the_driver_to_dial() {
        let ctx = context(
            "postgres",
            &[],
            fixed(&["8.8.8.8", "8.8.8.8", "2606:4700::1111"]),
        );

        let ips = ctx.resolve_checked("db.test").await.unwrap();

        assert_eq!(
            ips,
            vec![
                "8.8.8.8".parse::<IpAddr>().unwrap(),
                "2606:4700::1111".parse().unwrap()
            ]
        );
    }

    #[tokio::test]
    async fn a_private_answer_is_refused_with_egress_and_a_mixed_one_whole() {
        let private = context("postgres", &[], fixed(&["10.0.0.5"]));
        let mixed = context("postgres", &[], fixed(&["8.8.8.8", "10.0.0.5"]));

        for ctx in [private, mixed] {
            let error = ctx.resolve_checked("db.test").await.unwrap_err();
            assert!(
                matches!(
                    error,
                    SourceError::Egress(HttpError::EgressDenied { ref host, .. }) if host == "db.test"
                ),
                "{error:?}"
            );
        }
    }

    #[tokio::test]
    async fn an_allowlisted_range_is_returned() {
        let ctx = context("postgres", &["10.0.0.0/8"], fixed(&["10.0.0.5"]));

        let ips = ctx.resolve_checked("db.test").await.unwrap();

        assert_eq!(ips, vec!["10.0.0.5".parse::<IpAddr>().unwrap()]);
    }

    #[tokio::test]
    async fn an_ip_literal_skips_dns_and_is_still_checked() {
        let allowed = context("postgres", &["127.0.0.1/32"], Arc::new(Never));
        let denied = context("postgres", &[], Arc::new(Never));

        assert_eq!(
            allowed.resolve_checked("127.0.0.1").await.unwrap(),
            vec!["127.0.0.1".parse::<IpAddr>().unwrap()]
        );
        assert!(matches!(
            denied.resolve_checked("127.0.0.1").await,
            Err(SourceError::Egress(_))
        ));
    }

    #[tokio::test]
    async fn an_empty_answer_is_unreachable_and_more_than_eight_addresses_are_refused() {
        let empty = context("postgres", &[], fixed(&[]));
        let many: Vec<String> = (1..=9).map(|n| format!("8.8.8.{n}")).collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        let nine = context("postgres", &[], fixed(&many));
        let eight = context("postgres", &[], fixed(&many[..8]));

        assert_eq!(
            empty.resolve_checked("db.test").await,
            Err(SourceError::Unreachable)
        );
        assert!(matches!(
            nine.resolve_checked("db.test").await,
            Err(SourceError::Config { ref field, .. }) if field == "host"
        ));
        assert_eq!(eight.resolve_checked("db.test").await.unwrap().len(), 8);
    }

    #[tokio::test]
    async fn a_denied_address_past_the_cut_is_not_hidden_by_it() {
        let mut ips: Vec<String> = (1..=8).map(|n| format!("8.8.8.{n}")).collect();
        ips.push("10.0.0.5".to_owned());
        let ips: Vec<&str> = ips.iter().map(String::as_str).collect();
        let ctx = context("postgres", &[], fixed(&ips));

        let error = ctx.resolve_checked("db.test").await.unwrap_err();

        assert!(matches!(error, SourceError::Egress(_)), "{error:?}");
    }

    #[tokio::test]
    async fn a_denial_is_counted_by_class() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let ctx = context("postgres", &[], fixed(&["10.0.0.5"]));

        assert!(ctx.resolve_checked("db.test").await.is_err());

        let rendered = handle.render();
        assert!(
            rendered.contains("zirv_flow_egress_denied_total{class=\"private\"} 1"),
            "{rendered}"
        );
    }
}
