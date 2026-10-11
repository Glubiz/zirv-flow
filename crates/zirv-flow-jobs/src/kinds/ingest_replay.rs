//! `ingest.replay`: judges rejected events again after a schema or policy fix and stores the
//! ones that now pass (architecture section 12.2). The payload is `{workspace_id, ids,
//! emit_triggers}`; the job's trace names why, a manual request or a definition change. Each
//! reject is replayed in a transaction of its own, so a retry after a failure finds the
//! finished ones already replayed and stores nothing twice.
//!
//! Nothing here logs an identifier, a value or an event name: only counts.

use serde_json::Value;
use tracing::info;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_store::{REPLAY_JOB_KIND, replay_rejects};

use crate::{JobContext, JobError, JobKind};

pub(crate) struct IngestReplay;

impl JobKind for IngestReplay {
    const KIND: &'static str = REPLAY_JOB_KIND;

    async fn run(&self, ctx: &mut JobContext<'_>, payload: Value) -> Result<(), JobError> {
        let (workspace, ids, emit_triggers) = parse(&payload)?;
        if ctx.workspace_id() != Some(workspace) {
            return Err(JobError::new(
                "the payload names another workspace than the job",
            ));
        }
        let settings = ctx
            .ingest()
            .ok_or(JobError::new("the worker has no pool for the replay"))?;
        let pool = settings.pool.clone();
        let key = settings
            .redaction_key
            .clone()
            .ok_or(JobError::new("the worker has no master key"))?;
        let trace = ctx.trace().clone();
        let totals = replay_rejects(&pool, workspace, &key, &ids, &trace, emit_triggers).await?;
        info!(
            stored = totals.stored(),
            flagged = totals.flagged,
            duplicate = totals.duplicate,
            still_rejected = totals.still_rejected,
            skipped = totals.skipped,
            "reject replay ran"
        );
        Ok(())
    }
}

fn parse(payload: &Value) -> Result<(WorkspaceId, Vec<Uuid>, bool), JobError> {
    let bad = JobError::new("the payload needs a workspace_id and ids");
    let workspace = payload
        .get("workspace_id")
        .and_then(Value::as_str)
        .and_then(|text| text.parse::<WorkspaceId>().ok())
        .ok_or(bad)?;
    let ids = payload
        .get("ids")
        .and_then(Value::as_array)
        .ok_or(bad)?
        .iter()
        .map(|id| id.as_str().and_then(|text| Uuid::parse_str(text).ok()))
        .collect::<Option<Vec<_>>>()
        .ok_or(bad)?;
    let emit_triggers = payload
        .get("emit_triggers")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok((workspace, ids, emit_triggers))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use sqlx::PgPool;
    use zirv_flow_core::events::{EventMode, PropertyType, SchemaProperty};
    use zirv_flow_core::{Cause, HmacPurpose, MasterKey, Slug, TraceContext, WorkspaceName};
    use zirv_flow_store::{
        EventOutcome, EventRecord, NewEventSchema, apply_event_batch, create_event_schema,
        enqueue_replay, publish_schema_draft, save_schema_draft,
    };

    use super::*;
    use crate::context::IngestSettings;

    #[test]
    fn the_kind_name_is_ingest_replay() {
        assert_eq!(IngestReplay::KIND, "ingest.replay");
    }

    #[test]
    fn the_payload_is_checked() {
        let ws = WorkspaceId::new();
        let id = Uuid::now_v7();

        let ok = parse(&json!({"workspace_id": ws.as_uuid(), "ids": [id]})).unwrap();

        assert_eq!(ok, (ws, vec![id], false));
        assert!(parse(&json!({"ids": []})).is_err());
        assert!(parse(&json!({"workspace_id": ws.as_uuid(), "ids": ["x"]})).is_err());
        assert!(parse(&json!({"workspace_id": ws.as_uuid()})).is_err());
    }

    fn property(name: &str, property_type: PropertyType) -> SchemaProperty {
        SchemaProperty {
            name: name.to_owned(),
            property_type,
            required: true,
            enum_values: None,
            description: String::new(),
            pii: false,
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_job_replays_a_fixed_reject_once_even_when_run_twice(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let name = WorkspaceName::parse("ours").unwrap();
        let slug = Slug::parse("ours").unwrap();
        let ws = zirv_flow_store::create_workspace(&pool, &name, &slug)
            .await
            .unwrap()
            .id;
        create_event_schema(
            &pool,
            ws,
            &NewEventSchema {
                name: "order".into(),
                mode: EventMode::Block,
                description: String::new(),
                properties: vec![property("total", PropertyType::Number)],
            },
        )
        .await
        .unwrap();
        let mut tx = zirv_flow_store::begin(&pool).await.unwrap();
        publish_schema_draft(&mut tx, ws, "order", None)
            .await
            .unwrap();
        zirv_flow_store::commit(tx).await.unwrap();
        let key = std::sync::Arc::new(MasterKey::generate().hmac_key(HmacPurpose::Redaction));
        let record = |message: &str, total: Value| EventRecord {
            source: "web".into(),
            identifiers: vec![("email".into(), "a@example.com".into())],
            name: "order".into(),
            properties: json!({ "total": total }).as_object().cloned().unwrap(),
            timestamp: None,
            received_at: time::OffsetDateTime::now_utc(),
            message_id: Some(message.into()),
            trace: TraceContext::root(Cause::api("t")),
        };
        apply_event_batch(&pool, ws, &key, &[record("m-0", json!(1))])
            .await
            .unwrap();
        let outcomes = apply_event_batch(&pool, ws, &key, &[record("m-1", json!("ten"))])
            .await
            .unwrap();
        let EventOutcome::Rejected { reject_id, .. } = outcomes[0] else {
            panic!("not rejected");
        };
        let mut tx = zirv_flow_store::begin(&pool).await.unwrap();
        save_schema_draft(
            &mut *tx,
            ws,
            "order",
            &[property("total", PropertyType::String)],
        )
        .await
        .unwrap();
        publish_schema_draft(&mut tx, ws, "order", None)
            .await
            .unwrap();
        zirv_flow_store::commit(tx).await.unwrap();
        let trace = TraceContext::root(Cause::definition_change(Uuid::now_v7()));
        enqueue_replay(&pool, ws, &[reject_id], false, &trace)
            .await
            .unwrap();
        let claimed = zirv_flow_store::claim_job(
            &pool,
            "w",
            &[IngestReplay::KIND],
            Duration::from_secs(30),
            None,
        )
        .await
        .unwrap()
        .unwrap();

        for _ in 0..2 {
            let mut tx = zirv_flow_store::begin(&pool).await.unwrap();
            let mut ctx = JobContext::new(&mut tx, &claimed).with_ingest(IngestSettings {
                pool: pool.clone(),
                redaction_key: Some(key.clone()),
                batch_size: 10,
            });
            IngestReplay
                .run(&mut ctx, claimed.payload.clone())
                .await
                .unwrap();
        }

        let flags: Vec<Vec<String>> =
            sqlx::query_scalar("SELECT flags FROM events WHERE message_id = 'm-1'")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(flags.len(), 1);
        assert!(flags[0].contains(&"replayed".to_owned()));
        assert!(flags[0].contains(&"no_triggers".to_owned()));
    }
}
