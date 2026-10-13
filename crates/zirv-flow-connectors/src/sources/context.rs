//! The handle a source gets to the network. A source makes no call except through this.

use std::sync::{Arc, Mutex};

use zirv_flow_core::egress::EgressPolicy;

use crate::http::{GuardedClient, HttpError, HttpLimits, Lookup, SystemLookup};
use crate::runtime::{
    AccessToken, Auth, RateLimit, RateLimits, RequestSpec, RetryPolicy, RuntimeError,
    RuntimeResponse, TokenSlot, send,
};

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
    /// Resolves the names of sources that dial by IP themselves (Kafka).
    lookup: Arc<dyn Lookup>,
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
            kind,
            source_id: source_id.into(),
            rate_limits,
            rate_limit: None,
            retry: RetryPolicy::default(),
            token: Arc::default(),
            flight: Arc::default(),
            lookup: Arc::new(SystemLookup),
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

    /// Resolves names through `lookup` where a source dials by IP itself. The egress policy still
    /// applies to whatever it returns.
    pub fn with_lookup(mut self, lookup: Arc<dyn Lookup>) -> Self {
        self.lookup = lookup;
        self
    }

    pub(crate) fn lookup(&self) -> Arc<dyn Lookup> {
        Arc::clone(&self.lookup)
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

    pub(crate) fn store_token(&self, token: AccessToken) {
        let mut slot = self.slot();
        slot.current = Some(token.clone());
        slot.refreshed = Some(token);
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
}
