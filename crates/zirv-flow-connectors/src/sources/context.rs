//! The handle a source gets to the network. A source makes no call except through this.

use std::sync::Arc;

use zirv_flow_core::egress::EgressPolicy;

use crate::http::{GuardedClient, HttpError, HttpLimits};

/// The guarded HTTP client and the egress policy behind it. A source that speaks HTTP uses
/// [`SourceContext::http`]; one that resolves and connects by IP itself (database and Kafka
/// sources) must check every address against [`SourceContext::egress_policy`] first.
#[derive(Clone)]
pub struct SourceContext {
    http: GuardedClient,
    policy: Arc<EgressPolicy>,
}

impl SourceContext {
    /// Builds the guarded client for `policy`.
    pub fn new(policy: Arc<EgressPolicy>, limits: HttpLimits) -> Result<Self, HttpError> {
        let http = GuardedClient::new(Arc::clone(&policy), limits)?;
        Ok(Self { http, policy })
    }

    /// Wraps a client built elsewhere, for the same `policy`.
    pub fn from_client(http: GuardedClient, policy: Arc<EgressPolicy>) -> Self {
        Self { http, policy }
    }

    pub fn http(&self) -> &GuardedClient {
        &self.http
    }

    pub fn egress_policy(&self) -> &Arc<EgressPolicy> {
        &self.policy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_context_carries_the_policy_it_was_built_from() {
        let policy = Arc::new(EgressPolicy::default());

        let context = SourceContext::new(Arc::clone(&policy), HttpLimits::default()).unwrap();

        assert!(Arc::ptr_eq(context.egress_policy(), &policy));
        assert_eq!(context.http().limits(), HttpLimits::default());
    }
}
