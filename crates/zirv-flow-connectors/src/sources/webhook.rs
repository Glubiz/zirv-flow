//! The inbound webhook source kind: a marker with the `push` capability. A hook receives its
//! records at `POST /hooks/{id}` (the `webhooks` module of the API crate), so this module reads
//! nothing, imports no HTTP and no crypto, and exists so a hook is a source and its payload a
//! stream like any other (mapping, freshness, preview).

use futures_util::StreamExt;
use futures_util::stream::{self, BoxStream};
use serde_json::Value;

use super::context::SourceContext;
use super::credential::Credential;
use super::error::SourceError;
use super::source::Source;
use super::spec::{Capabilities, SourceSpec};
use super::types::{Batch, Checkpoint, StreamConfig, StreamDescriptor, SyncMode};

/// The name of a hook's one stream.
pub const PAYLOAD_STREAM: &str = "payload";

pub(crate) struct Webhook;

impl Source for Webhook {
    const SPEC: SourceSpec = SourceSpec {
        kind: "webhook",
        label: "Webhook",
        description: "Receives JSON documents that an external system posts to a signed URL.",
        config_fields: &[],
        credential_fields: &[],
        rate_limit: None,
        capabilities: Capabilities {
            push: true,
            incremental: false,
            full_refresh: true,
            emits_deletes: false,
        },
    };

    async fn check(
        &self,
        _ctx: &SourceContext,
        _config: &Value,
        _credential: &Credential,
    ) -> Result<(), SourceError> {
        Ok(())
    }

    async fn discover(
        &self,
        _ctx: &SourceContext,
        _config: &Value,
        _credential: &Credential,
    ) -> Result<Vec<StreamDescriptor>, SourceError> {
        Ok(vec![StreamDescriptor {
            name: PAYLOAD_STREAM.to_owned(),
            fields: Vec::new(),
            sync_modes: vec![SyncMode::FullRefresh],
            default_cursor_field: None,
            default_primary_key: Vec::new(),
        }])
    }

    fn read<'a>(
        &'a self,
        _ctx: &'a SourceContext,
        _config: &'a Value,
        _credential: &'a Credential,
        _stream: &'a StreamConfig,
        _checkpoint: Option<Checkpoint>,
    ) -> BoxStream<'a, Result<Batch, SourceError>> {
        stream::empty().boxed()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;
    use zirv_flow_core::egress::EgressPolicy;

    use super::*;
    use crate::http::HttpLimits;

    fn context() -> SourceContext {
        SourceContext::new(
            Arc::new(EgressPolicy::default()),
            HttpLimits::default(),
            "webhook",
            "source-1",
            Arc::default(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn check_passes_and_discovery_reports_the_one_payload_stream() {
        let ctx = context();
        let credential = Credential::new();
        Webhook.check(&ctx, &json!({}), &credential).await.unwrap();
        let streams = Webhook
            .discover(&ctx, &json!({}), &credential)
            .await
            .unwrap();

        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].name, PAYLOAD_STREAM);
        assert!(streams[0].fields.is_empty());
        assert_eq!(streams[0].sync_modes, vec![SyncMode::FullRefresh]);
        assert!(streams[0].default_primary_key.is_empty());
    }

    #[tokio::test]
    async fn read_yields_nothing() {
        let ctx = context();
        let stream = StreamConfig {
            name: PAYLOAD_STREAM.to_owned(),
            sync_mode: SyncMode::FullRefresh,
            cursor_field: None,
            primary_key: Vec::new(),
        };
        let batches: Vec<_> = Webhook
            .read(&ctx, &json!({}), &Credential::new(), &stream, None)
            .collect()
            .await;

        assert!(batches.is_empty());
    }

    #[test]
    fn the_module_imports_no_http_and_no_crypto() {
        let production = include_str!("webhook.rs");
        let production = production.split("#[cfg(test)]").next().unwrap();
        for word in ["reqwest", "hmac", "sha2", "crate::http"] {
            assert!(!production.contains(&format!("use {word}")), "{word}");
            assert!(!production.contains(&format!("{word}::")), "{word}");
        }
    }
}
