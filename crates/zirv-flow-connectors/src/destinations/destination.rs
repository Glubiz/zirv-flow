//! The `Destination` trait and its object-safe form.

use std::future::Future;
use std::pin::Pin;

use serde_json::{Map, Value};
use zirv_flow_core::flow::SyncOperation;

use super::context::DestinationContext;
use super::dkim::DkimSigner;
use super::error::DestinationError;
use super::spec::DestinationSpec;
use crate::sources::Credential;

/// One message to deliver. It lives in memory for the call only: nothing here is stored, and it
/// has no `Debug`, so the address cannot reach a log line by accident.
pub struct Outbound<'a> {
    /// Stable across every retry of this message; passed on where the type supports it.
    pub idempotency_key: &'a str,
    /// The channel, one of the type's [`Capabilities::channels`](super::Capabilities::channels).
    pub channel: &'a str,
    /// The address on that channel.
    pub to: &'a str,
    /// The rendered content, in the shape the type's channel expects.
    pub payload: &'a Value,
    /// The sender domain's DKIM key, for a type whose capabilities say it signs; none for the
    /// others, and none when the sender has no key. The destination never decrypts anything.
    pub dkim: Option<&'a DkimSigner>,
}

/// One profile operation to run at a remote system (`action.sync`, architecture 10.2). Like
/// [`Outbound`] it lives in memory for the call only and has no `Debug`: the key and the traits
/// are the person's data.
pub struct ProfileCall<'a> {
    pub operation: SyncOperation,
    /// The external key of the person at the remote system.
    pub key: &'a str,
    /// The traits the node lists, each at the person's value (`null` when they have none). Empty
    /// for a remove.
    pub traits: &'a Map<String, Value>,
    /// Stable across every retry of the node's effect; passed on in the operation's header.
    pub idempotency_key: &'a str,
}

/// A delivery the remote system accepted.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Delivered {
    /// The HTTP status of the accepting answer, when the type speaks HTTP.
    pub status: Option<u16>,
    /// The id the remote system gave the message, when it gives one.
    pub remote_id: Option<String>,
}

/// A type of external system messages are delivered to. The implementation holds no state: the
/// configuration, the credential and the message arrive with every call.
pub trait Destination: Send + Sync + 'static {
    /// What the type declares about itself.
    const SPEC: DestinationSpec;

    /// Proves the configuration and the credential work, without delivering anything.
    fn check(
        &self,
        ctx: &DestinationContext,
        config: &Value,
        credential: &Credential,
    ) -> impl Future<Output = Result<(), DestinationError>> + Send;

    /// Delivers one message. The caller holds a rate-bucket token and records the attempt.
    fn deliver(
        &self,
        ctx: &DestinationContext,
        config: &Value,
        credential: &Credential,
        message: &Outbound<'_>,
    ) -> impl Future<Output = Result<Delivered, DestinationError>> + Send;

    /// Runs one profile operation. Only a type whose capabilities list the `profile_sync` channel
    /// overrides it, and it may support one operation and not the other.
    fn sync(
        &self,
        ctx: &DestinationContext,
        config: &Value,
        credential: &Credential,
        call: &ProfileCall<'_>,
    ) -> impl Future<Output = Result<Delivered, DestinationError>> + Send {
        async move {
            let _ = (ctx, config, credential, call);
            Err(DestinationError::Unsupported)
        }
    }

    /// Erases the data held about `subject` (the type's own key for the person) at the remote
    /// system. Only a type with [`Capabilities::erase`](super::Capabilities::erase) overrides it.
    fn erase(
        &self,
        ctx: &DestinationContext,
        config: &Value,
        credential: &Credential,
        subject: &str,
    ) -> impl Future<Output = Result<(), DestinationError>> + Send {
        async move {
            let _ = (ctx, config, credential, subject);
            Err(DestinationError::Unsupported)
        }
    }
}

/// A boxed future, for the erased form.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// [`Destination`] without the associated constant and the `impl Future` returns, so
/// destinations can sit in one list.
pub trait ErasedDestination: Send + Sync {
    fn spec(&self) -> &'static DestinationSpec;

    fn check<'a>(
        &'a self,
        ctx: &'a DestinationContext,
        config: &'a Value,
        credential: &'a Credential,
    ) -> BoxFuture<'a, Result<(), DestinationError>>;

    fn deliver<'a>(
        &'a self,
        ctx: &'a DestinationContext,
        config: &'a Value,
        credential: &'a Credential,
        message: &'a Outbound<'a>,
    ) -> BoxFuture<'a, Result<Delivered, DestinationError>>;

    fn sync<'a>(
        &'a self,
        ctx: &'a DestinationContext,
        config: &'a Value,
        credential: &'a Credential,
        call: &'a ProfileCall<'a>,
    ) -> BoxFuture<'a, Result<Delivered, DestinationError>>;

    fn erase<'a>(
        &'a self,
        ctx: &'a DestinationContext,
        config: &'a Value,
        credential: &'a Credential,
        subject: &'a str,
    ) -> BoxFuture<'a, Result<(), DestinationError>>;
}

impl<D: Destination> ErasedDestination for D {
    fn spec(&self) -> &'static DestinationSpec {
        &D::SPEC
    }

    fn check<'a>(
        &'a self,
        ctx: &'a DestinationContext,
        config: &'a Value,
        credential: &'a Credential,
    ) -> BoxFuture<'a, Result<(), DestinationError>> {
        Box::pin(Destination::check(self, ctx, config, credential))
    }

    fn deliver<'a>(
        &'a self,
        ctx: &'a DestinationContext,
        config: &'a Value,
        credential: &'a Credential,
        message: &'a Outbound<'a>,
    ) -> BoxFuture<'a, Result<Delivered, DestinationError>> {
        Box::pin(Destination::deliver(self, ctx, config, credential, message))
    }

    fn sync<'a>(
        &'a self,
        ctx: &'a DestinationContext,
        config: &'a Value,
        credential: &'a Credential,
        call: &'a ProfileCall<'a>,
    ) -> BoxFuture<'a, Result<Delivered, DestinationError>> {
        Box::pin(Destination::sync(self, ctx, config, credential, call))
    }

    fn erase<'a>(
        &'a self,
        ctx: &'a DestinationContext,
        config: &'a Value,
        credential: &'a Credential,
        subject: &'a str,
    ) -> BoxFuture<'a, Result<(), DestinationError>> {
        Box::pin(Destination::erase(self, ctx, config, credential, subject))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;
    use zirv_flow_core::egress::EgressPolicy;

    use super::super::spec::Capabilities;
    use super::*;
    use crate::http::HttpLimits;

    struct Empty;

    impl Destination for Empty {
        const SPEC: DestinationSpec = DestinationSpec {
            kind: "empty",
            label: "Empty",
            description: "",
            config_fields: &[],
            credential_fields: &[],
            capabilities: Capabilities {
                channels: &["email"],
                idempotency_key: false,
                batching: false,
                rate_limit: false,
                erase: false,
                signs_dkim: false,
            },
            config_check: None,
        };

        async fn check(
            &self,
            _: &DestinationContext,
            _: &Value,
            _: &Credential,
        ) -> Result<(), DestinationError> {
            Ok(())
        }

        async fn deliver(
            &self,
            _: &DestinationContext,
            _: &Value,
            _: &Credential,
            message: &Outbound<'_>,
        ) -> Result<Delivered, DestinationError> {
            Ok(Delivered {
                status: Some(202),
                remote_id: Some(message.idempotency_key.to_owned()),
            })
        }
    }

    #[tokio::test]
    async fn the_erased_form_forwards_the_spec_and_every_call_and_erase_defaults_to_unsupported() {
        let destination: &dyn ErasedDestination = &Empty;
        let ctx = DestinationContext::new(
            Arc::new(EgressPolicy::default()),
            HttpLimits::default(),
            "empty",
            "destination-1",
        )
        .unwrap();
        let config = json!({});
        let credential = Credential::new();
        let payload = json!({"subject": "hi"});
        let message = Outbound {
            idempotency_key: "key-1",
            channel: "email",
            to: "a@example.test",
            payload: &payload,
            dkim: None,
        };

        assert_eq!(destination.spec().kind, "empty");
        assert_eq!(destination.check(&ctx, &config, &credential).await, Ok(()));
        assert_eq!(
            destination
                .deliver(&ctx, &config, &credential, &message)
                .await,
            Ok(Delivered {
                status: Some(202),
                remote_id: Some("key-1".into())
            })
        );
        assert_eq!(
            destination.erase(&ctx, &config, &credential, "p-1").await,
            Err(DestinationError::Unsupported)
        );
        let traits = Map::new();
        let call = ProfileCall {
            operation: SyncOperation::Upsert,
            key: "k",
            traits: &traits,
            idempotency_key: "key-1",
        };
        assert_eq!(
            destination.sync(&ctx, &config, &credential, &call).await,
            Err(DestinationError::Unsupported),
            "sync defaults to unsupported"
        );
    }
}
