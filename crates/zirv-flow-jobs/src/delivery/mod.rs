//! The delivery code every outbound job shares (architecture 10.1 and 14): one verdict function
//! that says what a destination's answer means, and one write of the `delivery_attempts` row.
//! `webhook.call` uses it now and `delivery.send` and the other destination jobs use it as they
//! arrive, so a retry rule or a row shape is changed in one place.
//!
//! - [`send`] calls a destination and judges the result into a [`Verdict`].
//! - [`settle`] records the attempt for a destination that has a row and counts it once the job
//!   transaction has committed. A call with no destination row, such as the raw URL of an
//!   `action.webhook` node, records nothing and touches no rate bucket.

pub(crate) mod attempt;
pub(crate) mod verdict;

pub(crate) use attempt::settle;
pub(crate) use verdict::{Verdict, verdict_of};

use std::time::{Duration, Instant};

use serde_json::Value;
use time::OffsetDateTime;
use zirv_flow_connectors::destinations::{DestinationContext, ErasedDestination, Outbound};
use zirv_flow_connectors::sources::Credential;

/// One call to a destination and how it went.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct Attempt {
    pub(crate) verdict: Verdict,
    /// How long the call took.
    pub(crate) elapsed: Duration,
}

/// Delivers `message` through `destination` and judges the answer at `now`.
pub(crate) async fn send(
    destination: &dyn ErasedDestination,
    ctx: &DestinationContext,
    config: &Value,
    credential: &Credential,
    message: &Outbound<'_>,
    now: OffsetDateTime,
) -> Attempt {
    let started = Instant::now();
    let result = destination.deliver(ctx, config, credential, message).await;
    Attempt {
        verdict: verdict_of(&result, now),
        elapsed: started.elapsed(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;
    use zirv_flow_connectors::destinations::Registry;
    use zirv_flow_connectors::http::HttpLimits;
    use zirv_flow_core::egress::EgressPolicy;

    use super::*;

    #[tokio::test]
    async fn a_destination_is_called_and_its_answer_judged() {
        let registry = Registry::built_in();
        let fake = registry.get("fake").expect("the fake is registered");
        let ctx = DestinationContext::new(
            Arc::new(EgressPolicy::default()),
            HttpLimits::default(),
            "fake",
            "destination-1",
        )
        .unwrap();
        let payload = json!({});
        let message = Outbound {
            idempotency_key: "key-1",
            channel: "email",
            to: "a@example.test",
            payload: &payload,
        };
        let now = OffsetDateTime::UNIX_EPOCH;

        for (mode, expected) in [
            ("ok", Verdict::Succeeded { status: 202 }),
            (
                "rejected",
                Verdict::Terminal {
                    status: Some(422),
                    reason: "http_4xx",
                },
            ),
        ] {
            let attempt = send(
                fake,
                &ctx,
                &json!({"mode": mode}),
                &Credential::new(),
                &message,
                now,
            )
            .await;

            assert_eq!(attempt.verdict, expected, "{mode}");
        }
    }
}
