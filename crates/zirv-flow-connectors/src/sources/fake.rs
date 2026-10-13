//! An in-memory source for tests of the sources API and UI. It makes no network call.
//! Registered only with the `test-support` feature.

use futures_util::StreamExt;
use futures_util::stream::{self, BoxStream};
use serde_json::{Value, json};

use super::context::SourceContext;
use super::credential::Credential;
use super::error::SourceError;
use super::source::Source;
use super::spec::{Capabilities, FieldKind, FieldSpec, SourceSpec};
use super::types::{
    Batch, Checkpoint, FieldDescriptor, FieldType, Record, StreamConfig, StreamDescriptor, SyncMode,
};

/// The only token `check` accepts.
pub const GOOD_TOKEN: &str = "ok";

/// How many records every stream holds, and how many one batch carries, unless the config says.
const RECORDS: u64 = 5;
const BATCH: u64 = 2;

pub(crate) struct Fake;

impl Source for Fake {
    const SPEC: SourceSpec = SourceSpec {
        kind: "fake",
        label: "Fake source",
        description: "An in-memory source for tests.",
        config_fields: &[
            FieldSpec {
                key: "streams",
                label: "Streams",
                help: "How many streams discovery reports.",
                kind: FieldKind::Number,
                required: false,
                secret: false,
            },
            FieldSpec {
                key: "fail_check",
                label: "Fail the check",
                help: "Makes the connection test fail.",
                kind: FieldKind::Bool,
                required: false,
                secret: false,
            },
            FieldSpec {
                key: "records",
                label: "Records",
                help: "How many records every stream holds (default 5).",
                kind: FieldKind::Number,
                required: false,
                secret: false,
            },
            FieldSpec {
                key: "batch",
                label: "Batch size",
                help: "How many records one batch carries (default 2).",
                kind: FieldKind::Number,
                required: false,
                secret: false,
            },
            FieldSpec {
                key: "deleted",
                label: "Deleted ids",
                help: "Comma-separated record ids emitted as deleted.",
                kind: FieldKind::Text,
                required: false,
                secret: false,
            },
            FieldSpec {
                key: "soft_deleted",
                label: "Soft-deleted ids",
                help: "Comma-separated record ids whose deleted_flag is true and whose deleted_at is set.",
                kind: FieldKind::Text,
                required: false,
                secret: false,
            },
            FieldSpec {
                key: "invalid",
                label: "Invalid ids",
                help: "Comma-separated record ids whose email is not an email.",
                kind: FieldKind::Text,
                required: false,
                secret: false,
            },
            FieldSpec {
                key: "fail_after_batches",
                label: "Fail after batches",
                help: "A read error after this many batches.",
                kind: FieldKind::Number,
                required: false,
                secret: false,
            },
        ],
        credential_fields: &[FieldSpec {
            key: "token",
            label: "Token",
            help: "The check passes only for the token ok.",
            kind: FieldKind::Text,
            required: true,
            secret: true,
        }],
        rate_limit: None,
        config_check: None,
        capabilities: Capabilities {
            incremental: true,
            full_refresh: true,
            emits_deletes: true,
        },
    };

    async fn check(
        &self,
        _ctx: &SourceContext,
        config: &Value,
        credential: &Credential,
    ) -> Result<(), SourceError> {
        if config["fail_check"].as_bool().unwrap_or(false) {
            return Err(SourceError::Unreachable);
        }
        match credential.get("token") {
            Some(token) if token.expose() == GOOD_TOKEN => Ok(()),
            _ => Err(SourceError::Auth),
        }
    }

    async fn discover(
        &self,
        _ctx: &SourceContext,
        config: &Value,
        _credential: &Credential,
    ) -> Result<Vec<StreamDescriptor>, SourceError> {
        let count = config["streams"].as_u64().unwrap_or(1).min(50);
        Ok((1..=count)
            .map(|n| StreamDescriptor {
                name: format!("stream_{n}"),
                fields: vec![
                    FieldDescriptor {
                        name: "id".into(),
                        field_type: FieldType::Number,
                    },
                    FieldDescriptor {
                        name: "name".into(),
                        field_type: FieldType::String,
                    },
                    FieldDescriptor {
                        name: "email".into(),
                        field_type: FieldType::String,
                    },
                    FieldDescriptor {
                        name: "updated_at".into(),
                        field_type: FieldType::Timestamp,
                    },
                    FieldDescriptor {
                        name: "deleted_flag".into(),
                        field_type: FieldType::String,
                    },
                    FieldDescriptor {
                        name: "deleted_at".into(),
                        field_type: FieldType::Timestamp,
                    },
                ],
                sync_modes: vec![SyncMode::FullRefresh, SyncMode::Incremental],
                default_cursor_field: Some("updated_at".into()),
                default_primary_key: vec!["id".into()],
            })
            .collect())
    }

    fn read<'a>(
        &'a self,
        _ctx: &'a SourceContext,
        config: &'a Value,
        _credential: &'a Credential,
        stream: &'a StreamConfig,
        checkpoint: Option<Checkpoint>,
    ) -> BoxStream<'a, Result<Batch, SourceError>> {
        let records = config["records"].as_u64().unwrap_or(RECORDS).min(100_000);
        let batch = config["batch"].as_u64().unwrap_or(BATCH).clamp(1, 100_000);
        let deleted = id_set(&config["deleted"]);
        let invalid = id_set(&config["invalid"]);
        let soft_deleted = id_set(&config["soft_deleted"]);
        let fail_after = config["fail_after_batches"].as_u64();
        let start = checkpoint
            .and_then(|checkpoint| checkpoint.0["offset"].as_u64())
            .unwrap_or(0)
            .min(records);
        let mut batches = Vec::new();
        let mut offset = start;
        while offset < records {
            if fail_after == Some(batches.len() as u64) {
                batches.push(Err(SourceError::Other("the fake source failed".into())));
                break;
            }
            let end = (offset + batch).min(records);
            let batch_records = (offset..end)
                .map(|id| Record {
                    key: format!("{}-{id}", stream.name),
                    data: json!({
                        "id": id,
                        "name": format!("{} record {id}", stream.name),
                        "email": if invalid.contains(&id) {
                            "not-an-email".to_owned()
                        } else {
                            format!("{}-{id}@example.com", stream.name)
                        },
                        "updated_at": format!("2026-01-{:02}T00:00:00Z", id % 28 + 1),
                        "deleted_flag": if soft_deleted.contains(&id) { "true" } else { "false" },
                        "deleted_at": if soft_deleted.contains(&id) { "2026-02-01T00:00:00Z" } else { "" },
                    }),
                    deleted: deleted.contains(&id),
                    occurrence: None,
                })
                .collect();
            batches.push(Ok(Batch {
                records: batch_records,
                checkpoint: Checkpoint(json!({"offset": end})),
                refused: Vec::new(),
            }));
            offset = end;
        }
        stream::iter(batches).boxed()
    }
}

/// The ids of a comma-separated text, or of an array of numbers.
fn id_set(value: &Value) -> Vec<u64> {
    match value {
        Value::String(text) => text
            .split(',')
            .filter_map(|part| part.trim().parse().ok())
            .collect(),
        Value::Array(items) => items.iter().filter_map(Value::as_u64).collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use zirv_flow_core::SecretString;
    use zirv_flow_core::egress::EgressPolicy;

    use super::*;
    use crate::http::HttpLimits;

    fn context() -> SourceContext {
        SourceContext::new(
            Arc::new(EgressPolicy::default()),
            HttpLimits::default(),
            "fake",
            "source-1",
            Arc::default(),
        )
        .unwrap()
    }

    fn credential(token: &str) -> Credential {
        let mut credential = Credential::new();
        credential.insert("token", SecretString::new(token.into()));
        credential
    }

    fn stream() -> StreamConfig {
        StreamConfig {
            name: "stream_1".into(),
            sync_mode: SyncMode::Incremental,
            cursor_field: Some("updated_at".into()),
            primary_key: vec!["id".into()],
        }
    }

    #[tokio::test]
    async fn check_passes_only_for_the_good_token_and_an_unset_fail_flag() {
        let ctx = context();

        assert_eq!(
            Fake.check(&ctx, &json!({}), &credential("ok")).await,
            Ok(())
        );
        assert_eq!(
            Fake.check(&ctx, &json!({}), &credential("no")).await,
            Err(SourceError::Auth)
        );
        assert_eq!(
            Fake.check(&ctx, &json!({}), &Credential::new()).await,
            Err(SourceError::Auth)
        );
        assert_eq!(
            Fake.check(&ctx, &json!({"fail_check": true}), &credential("ok"))
                .await,
            Err(SourceError::Unreachable)
        );
    }

    #[tokio::test]
    async fn discover_reports_the_configured_number_of_streams() {
        let ctx = context();

        let one = Fake
            .discover(&ctx, &json!({}), &Credential::new())
            .await
            .unwrap();
        let three = Fake
            .discover(&ctx, &json!({"streams": 3}), &Credential::new())
            .await
            .unwrap();

        assert_eq!(one.len(), 1);
        let names: Vec<_> = three.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["stream_1", "stream_2", "stream_3"]);
        assert_eq!(three[0].default_cursor_field.as_deref(), Some("updated_at"));
    }

    #[tokio::test]
    async fn read_is_deterministic_and_resumes_from_a_checkpoint() {
        let ctx = context();
        let config = json!({});
        let credential = Credential::new();
        let stream = stream();

        let all: Vec<Batch> = Fake
            .read(&ctx, &config, &credential, &stream, None)
            .map(|batch| batch.unwrap())
            .collect()
            .await;
        let again: Vec<Batch> = Fake
            .read(&ctx, &config, &credential, &stream, None)
            .map(|batch| batch.unwrap())
            .collect()
            .await;
        let resumed: Vec<Batch> = Fake
            .read(
                &ctx,
                &config,
                &credential,
                &stream,
                Some(all[0].checkpoint.clone()),
            )
            .map(|batch| batch.unwrap())
            .collect()
            .await;

        assert_eq!(all, again);
        let sizes: Vec<_> = all.iter().map(|batch| batch.records.len()).collect();
        assert_eq!(sizes, [2, 2, 1]);
        assert_eq!(all[2].checkpoint, Checkpoint(json!({"offset": 5})));
        assert_eq!(resumed, all[1..]);
        assert_eq!(all[0].records[0].key, "stream_1-0");
    }

    #[tokio::test]
    async fn the_config_knobs_shape_size_deletes_invalid_records_and_failures() {
        let ctx = context();
        let credential = Credential::new();
        let stream = stream();
        let config = json!({"records": 7, "batch": 3, "deleted": "1, 4", "invalid": [2]});

        let batches: Vec<Batch> = Fake
            .read(&ctx, &config, &credential, &stream, None)
            .map(|batch| batch.unwrap())
            .collect()
            .await;

        let sizes: Vec<_> = batches.iter().map(|batch| batch.records.len()).collect();
        assert_eq!(sizes, [3, 3, 1]);
        let all: Vec<&Record> = batches.iter().flat_map(|b| &b.records).collect();
        let deleted: Vec<_> = all.iter().filter(|r| r.deleted).map(|r| &r.key).collect();
        assert_eq!(deleted, ["stream_1-1", "stream_1-4"]);
        assert_eq!(all[2].data["email"], "not-an-email");
        assert_eq!(all[0].data["email"], "stream_1-0@example.com");

        let soft = json!({"records": 3, "soft_deleted": "1"});
        let batches: Vec<Batch> = Fake
            .read(&ctx, &soft, &credential, &stream, None)
            .map(|batch| batch.unwrap())
            .collect()
            .await;
        let all: Vec<&Record> = batches.iter().flat_map(|b| &b.records).collect();
        assert_eq!(all[1].data["deleted_flag"], "true");
        assert_eq!(all[1].data["deleted_at"], "2026-02-01T00:00:00Z");
        assert_eq!(all[0].data["deleted_flag"], "false");
        assert_eq!(all[0].data["deleted_at"], "");
        assert!(all.iter().all(|record| !record.deleted));

        let failing = json!({"fail_after_batches": 1});
        let results: Vec<_> = Fake
            .read(&ctx, &failing, &credential, &stream, None)
            .collect()
            .await;
        assert_eq!(results.len(), 2);
        assert!(results[0].is_ok());
        assert!(results[1].is_err());
    }
}
