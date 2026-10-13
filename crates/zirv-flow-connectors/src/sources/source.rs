//! The `Source` trait and its object-safe form.

use std::future::Future;
use std::pin::Pin;

use futures_util::stream::BoxStream;
use serde_json::Value;

use super::context::SourceContext;
use super::credential::Credential;
use super::error::SourceError;
use super::spec::SourceSpec;
use super::types::{Batch, Checkpoint, StreamConfig, StreamDescriptor};

/// A type of external system records are read from. The implementation holds no state: the
/// configuration, the credential and the checkpoint arrive with every call.
pub trait Source: Send + Sync + 'static {
    /// What the type declares about itself.
    const SPEC: SourceSpec;

    /// Proves the configuration and the credential work, without reading data.
    fn check(
        &self,
        ctx: &SourceContext,
        config: &Value,
        credential: &Credential,
    ) -> impl Future<Output = Result<(), SourceError>> + Send;

    /// Lists the streams the source offers.
    fn discover(
        &self,
        ctx: &SourceContext,
        config: &Value,
        credential: &Credential,
    ) -> impl Future<Output = Result<Vec<StreamDescriptor>, SourceError>> + Send;

    /// Reads one stream from `checkpoint` (from the start when `None`) as batches. Each batch
    /// carries the checkpoint that resumes after it, so a run can stop between any two.
    fn read<'a>(
        &'a self,
        ctx: &'a SourceContext,
        config: &'a Value,
        credential: &'a Credential,
        stream: &'a StreamConfig,
        checkpoint: Option<Checkpoint>,
    ) -> BoxStream<'a, Result<Batch, SourceError>>;
}

/// A boxed future, for the erased form.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// [`Source`] without the associated constant and the `impl Future` returns, so sources can sit
/// in one list.
pub trait ErasedSource: Send + Sync {
    fn spec(&self) -> &'static SourceSpec;

    fn check<'a>(
        &'a self,
        ctx: &'a SourceContext,
        config: &'a Value,
        credential: &'a Credential,
    ) -> BoxFuture<'a, Result<(), SourceError>>;

    fn discover<'a>(
        &'a self,
        ctx: &'a SourceContext,
        config: &'a Value,
        credential: &'a Credential,
    ) -> BoxFuture<'a, Result<Vec<StreamDescriptor>, SourceError>>;

    fn read<'a>(
        &'a self,
        ctx: &'a SourceContext,
        config: &'a Value,
        credential: &'a Credential,
        stream: &'a StreamConfig,
        checkpoint: Option<Checkpoint>,
    ) -> BoxStream<'a, Result<Batch, SourceError>>;
}

impl<S: Source> ErasedSource for S {
    fn spec(&self) -> &'static SourceSpec {
        &S::SPEC
    }

    fn check<'a>(
        &'a self,
        ctx: &'a SourceContext,
        config: &'a Value,
        credential: &'a Credential,
    ) -> BoxFuture<'a, Result<(), SourceError>> {
        Box::pin(Source::check(self, ctx, config, credential))
    }

    fn discover<'a>(
        &'a self,
        ctx: &'a SourceContext,
        config: &'a Value,
        credential: &'a Credential,
    ) -> BoxFuture<'a, Result<Vec<StreamDescriptor>, SourceError>> {
        Box::pin(Source::discover(self, ctx, config, credential))
    }

    fn read<'a>(
        &'a self,
        ctx: &'a SourceContext,
        config: &'a Value,
        credential: &'a Credential,
        stream: &'a StreamConfig,
        checkpoint: Option<Checkpoint>,
    ) -> BoxStream<'a, Result<Batch, SourceError>> {
        Source::read(self, ctx, config, credential, stream, checkpoint)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures_util::StreamExt;
    use serde_json::json;
    use zirv_flow_core::egress::EgressPolicy;

    use super::super::spec::Capabilities;
    use super::*;
    use crate::http::HttpLimits;

    struct Empty;

    impl Source for Empty {
        const SPEC: SourceSpec = SourceSpec {
            kind: "empty",
            label: "Empty",
            description: "",
            config_fields: &[],
            credential_fields: &[],
            rate_limit: None,
            capabilities: Capabilities {
                incremental: false,
                full_refresh: true,
                emits_deletes: false,
            },
        };

        async fn check(
            &self,
            _: &SourceContext,
            _: &Value,
            _: &Credential,
        ) -> Result<(), SourceError> {
            Ok(())
        }

        async fn discover(
            &self,
            _: &SourceContext,
            _: &Value,
            _: &Credential,
        ) -> Result<Vec<StreamDescriptor>, SourceError> {
            Err(SourceError::Unreachable)
        }

        fn read<'a>(
            &'a self,
            _: &'a SourceContext,
            _: &'a Value,
            _: &'a Credential,
            _: &'a StreamConfig,
            _: Option<Checkpoint>,
        ) -> BoxStream<'a, Result<Batch, SourceError>> {
            futures_util::stream::empty().boxed()
        }
    }

    #[tokio::test]
    async fn the_erased_form_forwards_the_spec_and_every_call() {
        let source: &dyn ErasedSource = &Empty;
        let ctx = SourceContext::new(
            Arc::new(EgressPolicy::default()),
            HttpLimits::default(),
            "empty",
            "source-1",
            Arc::default(),
        )
        .unwrap();
        let config = json!({});
        let credential = Credential::new();
        let stream = StreamConfig {
            name: "s".into(),
            sync_mode: super::super::types::SyncMode::FullRefresh,
            cursor_field: None,
            primary_key: vec![],
        };

        assert_eq!(source.spec().kind, "empty");
        assert_eq!(source.check(&ctx, &config, &credential).await, Ok(()));
        assert_eq!(
            source.discover(&ctx, &config, &credential).await,
            Err(SourceError::Unreachable)
        );
        let batches: Vec<_> = source
            .read(&ctx, &config, &credential, &stream, None)
            .collect()
            .await;
        assert!(batches.is_empty());
    }
}
