//! The handle a destination gets to the network. A destination makes no call except through
//! this.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use zirv_flow_core::egress::EgressPolicy;

use super::error::DestinationError;
use crate::http::{GuardedClient, HttpError, HttpLimits, Lookup, SystemLookup, check_all};

/// The most addresses [`DestinationContext::resolve_checked`] hands to a driver.
pub(crate) const MAX_RESOLVED: usize = 8;
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(10);

/// The guarded HTTP client and the egress policy behind it. A destination that speaks HTTP makes
/// its calls through [`DestinationContext::http`]; one that resolves and connects by IP itself
/// (a mail server) must get its addresses from [`DestinationContext::resolve_checked`], which
/// checks every one against [`DestinationContext::egress_policy`].
///
/// The rate bucket is not here: the delivery pipeline takes a token from the store before it
/// calls `deliver`, so an implementation never sees it.
#[derive(Clone)]
pub struct DestinationContext {
    http: GuardedClient,
    policy: Arc<EgressPolicy>,
    /// Resolves host names for the destinations that connect by address themselves.
    lookup: Arc<dyn Lookup>,
    /// The destination type: the label of the metrics.
    kind: &'static str,
    /// The destination's id.
    destination_id: String,
}

impl DestinationContext {
    /// Builds the guarded client for `policy`. `kind` is the destination type and
    /// `destination_id` the destination.
    pub fn new(
        policy: Arc<EgressPolicy>,
        limits: HttpLimits,
        kind: &'static str,
        destination_id: impl Into<String>,
    ) -> Result<Self, HttpError> {
        let http = GuardedClient::new(Arc::clone(&policy), limits)?;
        Ok(Self {
            http,
            policy,
            lookup: Arc::new(SystemLookup),
            kind,
            destination_id: destination_id.into(),
        })
    }

    /// Resolves names through `lookup` in [`DestinationContext::resolve_checked`] instead of the
    /// system resolver. For tests; the policy still applies to whatever `lookup` returns.
    pub fn with_lookup(mut self, lookup: Arc<dyn Lookup>) -> Self {
        self.lookup = lookup;
        self
    }

    /// Resolves `host` and checks every address against the egress policy. Only the returned
    /// addresses may be dialled, and the host name stays the TLS name. Call it at every connect,
    /// so a changed DNS answer is checked again.
    ///
    /// An IP literal skips DNS. An empty answer is `Unreachable`. One denied address refuses the
    /// whole host (counted in `zirv_flow_egress_denied_total`), and an answer of more than
    /// [`MAX_RESOLVED`] distinct addresses is refused only after all of them were checked, so the
    /// cut can never hide a denied one. A denial is [`Disposition::Terminal`] for the delivery.
    ///
    /// [`Disposition::Terminal`]: super::Disposition::Terminal
    pub async fn resolve_checked(&self, host: &str) -> Result<Vec<IpAddr>, DestinationError> {
        let mut ips = match host.parse::<IpAddr>() {
            Ok(ip) => vec![ip],
            Err(_) => match tokio::time::timeout(RESOLVE_TIMEOUT, self.lookup.lookup(host)).await {
                Ok(Ok(ips)) => ips,
                Ok(Err(_)) | Err(_) => return Err(DestinationError::Unreachable),
            },
        };
        let mut seen = Vec::with_capacity(ips.len());
        ips.retain(|ip| {
            let fresh = !seen.contains(ip);
            seen.push(*ip);
            fresh
        });
        if ips.is_empty() {
            return Err(DestinationError::Unreachable);
        }
        check_all(&self.policy, host, &ips)
            .map_err(|denied| DestinationError::Egress(denied.into()))?;
        if ips.len() > MAX_RESOLVED {
            return Err(DestinationError::Config {
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

    /// The destination type.
    pub fn kind(&self) -> &'static str {
        self.kind
    }

    /// The destination's id.
    pub fn destination_id(&self) -> &str {
        &self.destination_id
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::io;
    use std::pin::Pin;

    use super::super::error::Disposition;
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

    fn context(allow: &[&str], answer: &[&str]) -> DestinationContext {
        let entries: Vec<String> = allow.iter().map(|entry| (*entry).to_owned()).collect();
        DestinationContext::new(
            Arc::new(EgressPolicy::parse(&entries).unwrap()),
            HttpLimits::default(),
            "fake",
            "destination-1",
        )
        .unwrap()
        .with_lookup(Arc::new(Fixed(
            answer.iter().map(|ip| ip.parse().unwrap()).collect(),
        )))
    }

    #[test]
    fn a_context_carries_the_policy_the_type_and_the_id() {
        let policy = Arc::new(EgressPolicy::default());

        let ctx = DestinationContext::new(
            Arc::clone(&policy),
            HttpLimits::default(),
            "fake",
            "destination-1",
        )
        .unwrap();

        assert!(Arc::ptr_eq(ctx.egress_policy(), &policy));
        assert_eq!(ctx.http().limits(), HttpLimits::default());
        assert_eq!(ctx.kind(), "fake");
        assert_eq!(ctx.destination_id(), "destination-1");
    }

    #[tokio::test]
    async fn a_public_answer_is_returned_without_duplicates() {
        let ctx = context(&[], &["8.8.8.8", "8.8.8.8", "2606:4700::1111"]);

        let ips = ctx.resolve_checked("mail.test").await.unwrap();

        assert_eq!(
            ips,
            vec![
                "8.8.8.8".parse::<IpAddr>().unwrap(),
                "2606:4700::1111".parse().unwrap()
            ]
        );
    }

    #[tokio::test]
    async fn an_egress_denial_is_terminal_for_a_private_answer_a_mixed_one_and_a_literal() {
        let private = context(&[], &["10.0.0.5"]);
        let mixed = context(&[], &["8.8.8.8", "10.0.0.5"]);
        let literal = context(&[], &[]);

        for (ctx, host) in [
            (private, "mail.test"),
            (mixed, "mail.test"),
            (literal, "127.0.0.1"),
        ] {
            let error = ctx.resolve_checked(host).await.unwrap_err();

            assert!(
                matches!(
                    error,
                    DestinationError::Egress(HttpError::EgressDenied { .. })
                ),
                "{error:?}"
            );
            assert_eq!(error.disposition(), Disposition::Terminal);
        }
    }

    #[tokio::test]
    async fn an_allowlisted_range_is_returned() {
        let ctx = context(&["10.0.0.0/8"], &["10.0.0.5"]);

        assert_eq!(
            ctx.resolve_checked("mail.test").await.unwrap(),
            vec!["10.0.0.5".parse::<IpAddr>().unwrap()]
        );
    }

    #[tokio::test]
    async fn an_empty_answer_is_unreachable_and_nine_addresses_are_a_config_error() {
        let many: Vec<String> = (1..=9).map(|n| format!("8.8.8.{n}")).collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();

        assert_eq!(
            context(&[], &[]).resolve_checked("mail.test").await,
            Err(DestinationError::Unreachable)
        );
        assert!(matches!(
            context(&[], &many).resolve_checked("mail.test").await,
            Err(DestinationError::Config { ref field, .. }) if field == "host"
        ));
        assert_eq!(
            context(&[], &many[..8])
                .resolve_checked("mail.test")
                .await
                .unwrap()
                .len(),
            8
        );
    }
}
