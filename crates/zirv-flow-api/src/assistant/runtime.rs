//! What the API holds for model calls: the allowlist, the lazily built guarded client and, in
//! tests, a replacement provider.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use zirv_flow_ai::{AiError, AiHttp, AiHttpLimits, Provider, ProviderKind, provider_for};
use zirv_flow_core::egress::EgressPolicy;

use super::config::AiConfig;

/// Added to the client timeout before a pending call counts as abandoned.
const STALE_MARGIN: Duration = Duration::from_secs(60);

#[derive(Clone, Default)]
pub(crate) struct AiRuntime {
    pub(crate) config: Arc<AiConfig>,
    http: Arc<OnceLock<AiHttp>>,
    limits: AiHttpLimits,
    #[cfg(test)]
    replacement: Option<Arc<dyn Provider>>,
}

impl AiRuntime {
    pub(crate) fn new(config: AiConfig) -> Self {
        Self {
            config: Arc::new(config),
            ..Self::default()
        }
    }

    /// The guarded client, built on first use from the deployment's egress policy.
    pub(crate) fn http(&self, policy: &Arc<EgressPolicy>) -> Result<&AiHttp, AiError> {
        if let Some(http) = self.http.get() {
            return Ok(http);
        }
        let built = AiHttp::new(Arc::clone(policy), self.limits)?;
        Ok(self.http.get_or_init(|| built))
    }

    /// How long a reserved call may stay pending before it counts as abandoned: the client's
    /// whole timeout plus a margin for the bookkeeping after it.
    pub(crate) fn stale_after(&self) -> Duration {
        self.limits.timeout + STALE_MARGIN
    }

    /// The adapter for a protocol.
    pub(crate) fn provider(&self, kind: ProviderKind) -> Arc<dyn Provider> {
        #[cfg(test)]
        if let Some(provider) = &self.replacement {
            return Arc::clone(provider);
        }
        provider_for(kind)
    }

    /// Answers every call with `provider`, so tests never reach a model.
    #[cfg(test)]
    pub(crate) fn with_replacement(mut self, provider: Arc<dyn Provider>) -> Self {
        self.replacement = Some(provider);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zirv_flow_ai::fake::FakeProvider;

    #[test]
    fn the_client_is_built_once_and_shared() {
        let runtime = AiRuntime::default();
        let policy = Arc::new(EgressPolicy::default());

        let first = runtime.http(&policy).unwrap() as *const AiHttp;
        let second = runtime.clone().http(&policy).unwrap() as *const AiHttp;

        assert_eq!(first, second);
    }

    #[test]
    fn a_call_goes_stale_after_the_client_timeout_plus_a_margin() {
        let runtime = AiRuntime::default();
        assert_eq!(
            runtime.stale_after(),
            AiHttpLimits::default().timeout + STALE_MARGIN
        );
    }

    #[test]
    fn a_replacement_answers_for_every_kind_and_the_real_adapters_otherwise() {
        let plain = AiRuntime::default();
        assert_eq!(
            plain.provider(ProviderKind::Anthropic).kind(),
            ProviderKind::Anthropic
        );

        let fake = Arc::new(FakeProvider::new());
        let replaced = AiRuntime::default().with_replacement(fake);
        assert_eq!(
            replaced.provider(ProviderKind::Anthropic).kind(),
            ProviderKind::OpenAiCompatible
        );
    }
}
