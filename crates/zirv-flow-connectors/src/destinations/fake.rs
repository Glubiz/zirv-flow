//! An in-memory destination for tests of the destinations store, the API and the send pipeline.
//! It makes no network call. Registered only with the `test-support` feature.
//!
//! With a `ledger` set in its configuration it also honours idempotency keys like a destination
//! that supports them: the first delivery under a key is a message it accepted, and a later one
//! under the same key is answered as the same message and accepts nothing more. The ledger,
//! named by the setting, keeps every call and every accepted message, for a test to read with
//! [`ledger_calls`] and [`ledger_sent`].

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use serde_json::Value;

use super::context::DestinationContext;
use super::destination::{Delivered, Destination, Outbound};
use super::error::DestinationError;
use super::spec::{Capabilities, DestinationSpec};
use crate::sources::{Credential, FieldKind, FieldSpec};

/// The only token `check` accepts.
pub const GOOD_TOKEN: &str = "ok";

pub(crate) struct Fake;

/// A message the fake accepted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FakeSent {
    /// The idempotency key it was accepted under.
    pub key: String,
    /// The address it was sent to.
    pub to: String,
    /// The payload it carried.
    pub payload: Value,
}

#[derive(Default)]
struct Ledger {
    /// The idempotency key of every call, in order.
    calls: Vec<String>,
    /// The messages accepted, one per key.
    sent: Vec<FakeSent>,
}

static LEDGERS: Mutex<BTreeMap<String, Ledger>> = Mutex::new(BTreeMap::new());

/// The idempotency key of every call made to the ledger `name`, in order.
pub fn ledger_calls(name: &str) -> Vec<String> {
    LEDGERS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(name)
        .map(|ledger| ledger.calls.clone())
        .unwrap_or_default()
}

/// The messages the ledger `name` accepted, one per idempotency key, in order.
pub fn ledger_sent(name: &str) -> Vec<FakeSent> {
    LEDGERS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(name)
        .map(|ledger| ledger.sent.clone())
        .unwrap_or_default()
}

/// Records a call under `ledger` and says whether the key was new: a new key is a message
/// accepted.
fn record(ledger: &str, message: &Outbound<'_>) -> bool {
    let mut ledgers = LEDGERS.lock().unwrap_or_else(PoisonError::into_inner);
    let entry = ledgers.entry(ledger.to_owned()).or_default();
    entry.calls.push(message.idempotency_key.to_owned());
    let new = !entry
        .sent
        .iter()
        .any(|sent| sent.key == message.idempotency_key);
    if new {
        entry.sent.push(FakeSent {
            key: message.idempotency_key.to_owned(),
            to: message.to.to_owned(),
            payload: message.payload.clone(),
        });
    }
    new
}

impl Destination for Fake {
    const SPEC: DestinationSpec = DestinationSpec {
        kind: "fake",
        label: "Fake destination",
        description: "An in-memory destination for tests.",
        config_fields: &[
            FieldSpec {
                key: "host",
                label: "Host",
                help: "A host `check` and `deliver` resolve through the egress policy.",
                kind: FieldKind::Text,
                required: false,
                secret: false,
            },
            FieldSpec {
                key: "mode",
                label: "Mode",
                help: "How `deliver` answers.",
                kind: FieldKind::Select(&[
                    "ok",
                    "throttle",
                    "throttle_retry_after",
                    "server_error",
                    "rejected",
                    "lose_first_answer",
                ]),
                required: false,
                secret: false,
            },
            FieldSpec {
                key: "ledger",
                label: "Ledger",
                help: "A name under which `deliver` keeps the calls it received and the \
                       messages it accepted, one per idempotency key.",
                kind: FieldKind::Text,
                required: false,
                secret: false,
            },
        ],
        credential_fields: &[FieldSpec {
            key: "token",
            label: "Token",
            help: "Accepted by `check` only when it is `ok`.",
            kind: FieldKind::Text,
            required: true,
            secret: true,
        }],
        capabilities: Capabilities {
            channels: &["email"],
            idempotency_key: true,
            batching: false,
            rate_limit: true,
            erase: true,
            signs_dkim: false,
        },
        config_check: None,
    };

    async fn check(
        &self,
        ctx: &DestinationContext,
        config: &Value,
        credential: &Credential,
    ) -> Result<(), DestinationError> {
        resolve_host(ctx, config).await?;
        match credential.get("token") {
            Some(token) if token.expose() == GOOD_TOKEN => Ok(()),
            _ => Err(DestinationError::Auth),
        }
    }

    async fn deliver(
        &self,
        ctx: &DestinationContext,
        config: &Value,
        credential: &Credential,
        message: &Outbound<'_>,
    ) -> Result<Delivered, DestinationError> {
        resolve_host(ctx, config).await?;
        // A destination that wants a token (a test of the credential a send opens) refuses any
        // other.
        if let Some(wanted) = config.get("require_token").and_then(Value::as_str)
            && credential
                .get("token")
                .is_none_or(|token| token.expose() != wanted)
        {
            return Err(DestinationError::Upstream(401));
        }
        let mode = config.get("mode").and_then(Value::as_str).unwrap_or("ok");
        let ledger = config.get("ledger").and_then(Value::as_str);
        match mode {
            "throttle" => Err(DestinationError::Upstream(429)),
            "throttle_retry_after" => Err(DestinationError::Answered {
                status: 429,
                retry_after: Some("120".to_owned()),
            }),
            "server_error" => Err(DestinationError::Upstream(503)),
            "rejected" => Err(DestinationError::Upstream(422)),
            // The message is accepted and the answer is lost on its way back: the first call
            // under a key fails as a server error, and a retry is the same message.
            "lose_first_answer" => {
                let first = ledger.is_none_or(|ledger| record(ledger, message));
                if first {
                    Err(DestinationError::Upstream(503))
                } else {
                    Ok(accepted(message))
                }
            }
            _ => {
                if let Some(ledger) = ledger {
                    record(ledger, message);
                }
                Ok(accepted(message))
            }
        }
    }

    async fn erase(
        &self,
        ctx: &DestinationContext,
        config: &Value,
        _: &Credential,
        _: &str,
    ) -> Result<(), DestinationError> {
        resolve_host(ctx, config).await
    }
}

/// The answer of a destination that accepted the message.
fn accepted(message: &Outbound<'_>) -> Delivered {
    Delivered {
        status: Some(202),
        remote_id: Some(message.idempotency_key.to_owned()),
    }
}

/// Resolves the configured host, when there is one, through the context.
async fn resolve_host(ctx: &DestinationContext, config: &Value) -> Result<(), DestinationError> {
    match config.get("host").and_then(Value::as_str) {
        Some(host) => ctx.resolve_checked(host).await.map(|_| ()),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;
    use zirv_flow_core::SecretString;
    use zirv_flow_core::egress::EgressPolicy;

    use super::super::error::Disposition;
    use super::*;
    use crate::http::HttpLimits;

    fn ctx() -> DestinationContext {
        DestinationContext::new(
            Arc::new(EgressPolicy::default()),
            HttpLimits::default(),
            "fake",
            "destination-1",
        )
        .unwrap()
    }

    fn credential(token: &str) -> Credential {
        let mut credential = Credential::new();
        credential.insert("token", SecretString::new(token.to_owned()));
        credential
    }

    #[tokio::test]
    async fn check_accepts_only_the_good_token() {
        let config = json!({});

        assert_eq!(
            Fake.check(&ctx(), &config, &credential(GOOD_TOKEN)).await,
            Ok(())
        );
        assert_eq!(
            Fake.check(&ctx(), &config, &credential("bad")).await,
            Err(DestinationError::Auth)
        );
    }

    #[tokio::test]
    async fn deliver_answers_as_the_mode_says() {
        let payload = json!({});
        let message = Outbound {
            idempotency_key: "k",
            channel: "email",
            to: "a@example.test",
            payload: &payload,
            dkim: None,
        };
        let deliver = |mode: &'static str| {
            let config = json!({"mode": mode});
            let message = &message;
            async move {
                Fake.deliver(&ctx(), &config, &credential("x"), message)
                    .await
            }
        };

        assert_eq!(deliver("ok").await.unwrap().remote_id.as_deref(), Some("k"));
        assert_eq!(
            deliver("throttle").await.unwrap_err().disposition(),
            Disposition::Throttled { retry_after: None }
        );
        assert_eq!(
            deliver("server_error").await.unwrap_err().disposition(),
            Disposition::Retryable
        );
        assert_eq!(
            deliver("rejected").await.unwrap_err().disposition(),
            Disposition::Terminal
        );
    }

    #[tokio::test]
    async fn a_ledger_honours_idempotency_keys_and_keeps_every_call() {
        let payload = json!({"subject": "Hi"});
        let send = |key: &'static str, mode: &'static str| {
            let config = json!({"mode": mode, "ledger": "fake-ledger-test"});
            let payload = &payload;
            async move {
                let message = Outbound {
                    idempotency_key: key,
                    channel: "email",
                    to: "a@example.test",
                    payload,
                    dkim: None,
                };
                Fake.deliver(&ctx(), &config, &credential("x"), &message)
                    .await
            }
        };

        // The first call is accepted and its answer is lost; the retry is the same message.
        let lost = send("k1", "lose_first_answer").await.unwrap_err();
        let retried = send("k1", "lose_first_answer").await;
        let plain = send("k2", "ok").await;
        let again = send("k2", "ok").await;

        assert_eq!(lost.disposition(), Disposition::Retryable);
        assert!(retried.is_ok() && plain.is_ok() && again.is_ok());
        assert_eq!(
            ledger_calls("fake-ledger-test"),
            ["k1", "k1", "k2", "k2"],
            "every call is kept"
        );
        let sent = ledger_sent("fake-ledger-test");
        assert_eq!(
            sent.iter()
                .map(|sent| sent.key.as_str())
                .collect::<Vec<_>>(),
            ["k1", "k2"],
            "one message per key"
        );
        assert_eq!(sent[0].to, "a@example.test");
        assert_eq!(sent[0].payload, payload);
        assert!(ledger_sent("no-such-ledger").is_empty());
    }

    #[tokio::test]
    async fn an_answer_that_throttles_can_carry_a_retry_after() {
        let payload = json!({});
        let message = Outbound {
            idempotency_key: "k",
            channel: "email",
            to: "a@example.test",
            payload: &payload,
            dkim: None,
        };

        let error = Fake
            .deliver(
                &ctx(),
                &json!({"mode": "throttle_retry_after"}),
                &credential("x"),
                &message,
            )
            .await
            .unwrap_err();

        assert_eq!(
            error,
            DestinationError::Answered {
                status: 429,
                retry_after: Some("120".to_owned())
            }
        );
        assert_eq!(
            error.disposition(),
            Disposition::Throttled { retry_after: None }
        );
    }

    #[tokio::test]
    async fn a_private_host_is_an_egress_denial_that_is_terminal_for_check_deliver_and_erase() {
        let config = json!({"host": "10.0.0.5"});
        let payload = json!({});
        let message = Outbound {
            idempotency_key: "k",
            channel: "email",
            to: "a@example.test",
            payload: &payload,
            dkim: None,
        };
        let credential = credential(GOOD_TOKEN);

        let errors = [
            Fake.check(&ctx(), &config, &credential).await.unwrap_err(),
            Fake.deliver(&ctx(), &config, &credential, &message)
                .await
                .unwrap_err(),
            Fake.erase(&ctx(), &config, &credential, "p")
                .await
                .unwrap_err(),
        ];

        for error in errors {
            assert!(matches!(error, DestinationError::Egress(_)), "{error:?}");
            assert_eq!(error.disposition(), Disposition::Terminal);
        }
    }
}
