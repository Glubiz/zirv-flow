//! An in-memory destination for tests of the destinations store and API. It makes no network
//! call. Registered only with the `test-support` feature.

use serde_json::Value;

use super::context::DestinationContext;
use super::destination::{Delivered, Destination, Outbound};
use super::error::DestinationError;
use super::spec::{Capabilities, DestinationSpec};
use crate::sources::{Credential, FieldKind, FieldSpec};

/// The only token `check` accepts.
pub const GOOD_TOKEN: &str = "ok";

pub(crate) struct Fake;

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
                kind: FieldKind::Select(&["ok", "throttle", "server_error", "rejected"]),
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
        _: &Credential,
        message: &Outbound<'_>,
    ) -> Result<Delivered, DestinationError> {
        resolve_host(ctx, config).await?;
        match config.get("mode").and_then(Value::as_str).unwrap_or("ok") {
            "throttle" => Err(DestinationError::Upstream(429)),
            "server_error" => Err(DestinationError::Upstream(503)),
            "rejected" => Err(DestinationError::Upstream(422)),
            _ => Ok(Delivered {
                status: Some(202),
                remote_id: Some(message.idempotency_key.to_owned()),
            }),
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
    async fn a_private_host_is_an_egress_denial_that_is_terminal_for_check_deliver_and_erase() {
        let config = json!({"host": "10.0.0.5"});
        let payload = json!({});
        let message = Outbound {
            idempotency_key: "k",
            channel: "email",
            to: "a@example.test",
            payload: &payload,
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
