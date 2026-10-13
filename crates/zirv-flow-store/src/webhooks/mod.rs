//! Inbound webhooks (architecture sections 7.1 and 16.1). A hook is a source of kind `webhook`
//! with one stream `payload` and a `webhooks` row. This module holds the row's CRUD, the
//! receive path (look the hook up, append a verified payload to the inbox) and the drain's
//! mapping and apply steps for hook rows. Every function that reads or changes a hook by id is
//! scoped to one workspace, except [`find_for_receive`], which the unguessable URL id locates.
//!
//! Nothing here logs or labels anything with a payload value, a secret or a signature.

mod crud;
mod drain;
mod receive;

pub use crud::{
    NewWebhook, Webhook, WebhookUpdate, create_webhook, find_webhook, list_webhooks,
    replace_webhook_secret, update_webhook,
};
pub(crate) use drain::{
    HookConfigs, HookMapped, HookRejected, HookResult, apply_hook_record, map_hook_row,
};
pub use receive::{HookReceive, append_webhook, find_for_receive};

/// Fixtures for the tests of hooks: a workspace with a mapped, enabled hook.
#[cfg(test)]
pub(crate) mod fixtures {
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use uuid::Uuid;
    use zirv_flow_core::people::{Precedence, TraitType};
    use zirv_flow_core::{
        HmacKey, HmacPurpose, HmacRoot, Keyring, MasterKey, SecretId, SecretPurpose, Slug,
        TraceContext, WorkspaceId, WorkspaceName, seal, secret_aad,
    };

    use super::{NewWebhook, Webhook, create_webhook};
    use crate::{
        DiscoveredStream, NewSource, NewTraitDefinition, StreamSettings, begin, commit,
        configure_stream, create_source, create_trait_definition, create_workspace,
        set_stream_mapping, upsert_discovered_streams,
    };

    pub(crate) async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        crate::migrate(pool).await.unwrap();
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    pub(crate) fn key() -> HmacKey {
        HmacRoot::bootstrap_from(&MasterKey::generate()).hmac_key(HmacPurpose::Redaction)
    }

    pub(crate) fn trace() -> TraceContext {
        TraceContext::root(zirv_flow_core::Cause::api("test"))
    }

    /// The mapping every fixture hook starts with: an email identifier, the `plan` trait, an
    /// `order_placed` event with the nested `order.total`, and no list.
    pub(crate) fn mapping(lists: &[Uuid]) -> Value {
        let mut columns = vec![
            json!({"column": "email", "target": {"type": "identifier", "kind": "email"}}),
            json!({"column": "plan", "target": {"type": "trait", "key": "plan"}}),
        ];
        for list in lists {
            columns
                .push(json!({"column": "subscribed", "target": {"type": "list", "list_id": list}}));
        }
        json!({
            "columns": columns,
            "event": {"name": "order_placed", "properties": [{"column": "order.total", "property": "total"}]}
        })
    }

    pub(crate) async fn plan_trait(pool: &PgPool, ws: WorkspaceId) {
        create_trait_definition(
            pool,
            ws,
            &NewTraitDefinition {
                key: "plan".into(),
                trait_type: TraitType::String,
                precedence: Precedence::Latest,
                source_order: Vec::new(),
                pii: false,
                channel_address: false,
                inferred: false,
                description: String::new(),
            },
        )
        .await
        .unwrap();
    }

    /// A hook named `name` with `mapping` (none leaves the stream unmapped), enabled.
    pub(crate) async fn hook(
        pool: &PgPool,
        ws: WorkspaceId,
        name: &str,
        mapping: Option<&Value>,
    ) -> Webhook {
        let keys = Keyring::new(MasterKey::generate(), None).unwrap();
        let secret = SecretId::new();
        let sealed = seal(
            &keys,
            b"zfwh_test",
            &secret_aad(ws, secret, SecretPurpose::WebhookSecret),
        );
        crate::secrets::insert_unchecked(pool, ws, SecretPurpose::WebhookSecret, secret, &sealed)
            .await
            .unwrap();
        let config = json!({});
        let source = create_source(
            pool,
            ws,
            &NewSource {
                id: Uuid::now_v7(),
                kind: "webhook",
                name,
                config: &config,
                credential_secret_id: None,
                credential_fields: &[],
                schedule: None,
                created_by: None,
            },
        )
        .await
        .unwrap();
        let descriptor = json!({"name": "payload", "fields": []});
        let mut tx = begin(pool).await.unwrap();
        let streams = upsert_discovered_streams(
            &mut tx,
            ws,
            source.id,
            &[DiscoveredStream {
                name: "payload",
                sync_mode: "full_refresh",
                cursor_field: None,
                primary_key: &[],
                descriptor: &descriptor,
            }],
        )
        .await
        .unwrap()
        .unwrap();
        let stream = streams[0].id;
        let created = create_webhook(
            &mut tx,
            ws,
            &NewWebhook {
                id: Uuid::new_v4(),
                source_id: source.id,
                stream_id: stream,
                secret_id: secret,
                signature_header: "X-Zirv-Signature",
                signature_prefix: "sha256=",
                timestamp_header: Some("X-Zirv-Timestamp"),
                tolerance_seconds: 300,
                dedupe_field: "id",
                created_by: None,
            },
        )
        .await
        .unwrap();
        commit(tx).await.unwrap();
        configure_stream(
            &mut pool.acquire().await.unwrap(),
            ws,
            source.id,
            "payload",
            &StreamSettings {
                enabled: true,
                sync_mode: "full_refresh",
                cursor_field: None,
                primary_key: &[],
                staleness_threshold_seconds: Some(3600),
                reconcile_schedule: None,
                sweep_limit: 0.5,
            },
        )
        .await
        .unwrap()
        .unwrap();
        if let Some(mapping) = mapping {
            set_stream_mapping(pool, ws, source.id, "payload", mapping)
                .await
                .unwrap()
                .unwrap();
        }
        crate::find_webhook(pool, ws, created.id)
            .await
            .unwrap()
            .unwrap()
    }
}
