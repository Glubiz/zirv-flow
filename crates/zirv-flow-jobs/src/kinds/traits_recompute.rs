//! `traits.recompute`: brings the materialized values of one trait key back in line with its
//! definition after the precedence, the source order or the archived flag changed.
//!
//! The payload is `{workspace_id, key}`, plus a `after` person-id cursor on follow-up runs.
//! Each run covers one batch of people in its own transaction and enqueues the next batch
//! on the same trace until none is left.

use serde_json::{Value, json};
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_store::NewJob;

use crate::{JobContext, JobError, JobKind};

pub(crate) struct TraitsRecompute;

impl JobKind for TraitsRecompute {
    const KIND: &'static str = "traits.recompute";
    const NEEDS_CHANGE_READERS: bool = true;

    async fn run(&self, ctx: &mut JobContext<'_>, payload: Value) -> Result<(), JobError> {
        let (workspace, key, after) = parse(&payload)?;
        if ctx.workspace_id() != Some(workspace) {
            return Err(JobError::new(
                "the payload names another workspace than the job",
            ));
        }
        let trace = ctx.trace().clone();
        let job = Some((ctx.job_id(), ctx.attempts()));
        let next =
            zirv_flow_store::recompute_trait_batch(ctx.conn(), workspace, &key, after, &trace, job)
                .await?;
        if let Some(cursor) = next {
            let mut follow_up = NewJob::new(Self::KIND, trace);
            follow_up.workspace_id = Some(workspace);
            follow_up.payload = json!({ "workspace_id": workspace, "key": key, "after": cursor });
            ctx.enqueue(&follow_up).await?;
        }
        Ok(())
    }
}

fn parse(payload: &Value) -> Result<(WorkspaceId, String, Option<Uuid>), JobError> {
    let bad = JobError::new("the payload needs a workspace_id and a key");
    let workspace = payload
        .get("workspace_id")
        .and_then(Value::as_str)
        .and_then(|text| text.parse::<WorkspaceId>().ok())
        .ok_or(bad)?;
    let key = payload
        .get("key")
        .and_then(Value::as_str)
        .ok_or(bad)?
        .to_owned();
    let after = match payload.get("after") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_str()
                .and_then(|text| Uuid::parse_str(text).ok())
                .ok_or(JobError::new("the cursor is not a person id"))?,
        ),
    };
    Ok((workspace, key, after))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use sqlx::PgPool;
    use time::macros::datetime;
    use zirv_flow_core::{Cause, Slug, TraceContext, WorkspaceName};
    use zirv_flow_store::{
        ProfileRecord, TraitDefinitionUpdate, apply_profile_batch, find_person,
        update_trait_definition,
    };

    use super::*;

    #[test]
    fn the_kind_name_is_traits_recompute() {
        assert_eq!(TraitsRecompute::KIND, "traits.recompute");
    }

    #[test]
    fn the_payload_needs_a_workspace_and_a_key_and_may_carry_a_cursor() {
        let ws = WorkspaceId::new();
        let person = Uuid::now_v7();

        let first = parse(&json!({"workspace_id": ws, "key": "plan"})).unwrap();
        let next = parse(&json!({"workspace_id": ws, "key": "plan", "after": person})).unwrap();

        assert_eq!(first, (ws, "plan".to_owned(), None));
        assert_eq!(next.2, Some(person));
        assert!(parse(&json!({"key": "plan"})).is_err());
        assert!(parse(&json!({"workspace_id": ws})).is_err());
        assert!(parse(&json!({"workspace_id": ws, "key": "plan", "after": 5})).is_err());
    }

    fn record(source: &str, key: &str, tier: &str, month: u8) -> ProfileRecord {
        ProfileRecord {
            source: source.into(),
            stream: "s".into(),
            record_key: key.into(),
            identifiers: vec![("email".into(), "a@example.com".into())],
            traits: json!({ "tier": tier }).as_object().cloned().unwrap(),
            observed_at: datetime!(2026-01-01 00:00 UTC)
                .replace_month(time::Month::try_from(month).unwrap())
                .unwrap(),
            trace: TraceContext::root(Cause::api("t")),
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_job_applies_a_precedence_change_to_existing_people(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let name = WorkspaceName::parse("ours").unwrap();
        let slug = Slug::parse("ours").unwrap();
        let ws = zirv_flow_store::create_workspace(&pool, &name, &slug)
            .await
            .unwrap()
            .id;
        let outcomes = apply_profile_batch(
            &pool,
            ws,
            &[
                record("crm", "1", "crm-old", 1),
                record("shop", "2", "shop-new", 2),
            ],
        )
        .await
        .unwrap();
        let zirv_flow_store::ProfileOutcome::Applied { person_id, .. } = outcomes[0] else {
            panic!("not applied");
        };
        update_trait_definition(
            &pool,
            ws,
            "tier",
            &TraitDefinitionUpdate {
                precedence: Some(zirv_flow_core::people::Precedence::First),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let mut new = NewJob::new(TraitsRecompute::KIND, TraceContext::root(Cause::api("req")));
        new.workspace_id = Some(ws);
        new.payload = json!({"workspace_id": ws, "key": "tier"});
        zirv_flow_store::enqueue(&pool, &new).await.unwrap();
        let claimed = zirv_flow_store::claim_job(
            &pool,
            "w",
            &[TraitsRecompute::KIND],
            Duration::from_secs(30),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        let mut tx = zirv_flow_store::begin(&pool).await.unwrap();

        let mut ctx = JobContext::new(&mut tx, &claimed);
        TraitsRecompute
            .run(&mut ctx, claimed.payload.clone())
            .await
            .unwrap();
        zirv_flow_store::commit(tx).await.unwrap();

        let person = find_person(&pool, ws, person_id).await.unwrap().unwrap();
        assert_eq!(person.traits["tier"], "crm-old");
        let (records, _) = zirv_flow_store::list_trace(&pool, ws, claimed.trace.trace_id, 10)
            .await
            .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].job_id, Some(claimed.id));
        assert_eq!(
            records[0].subject_ids,
            json!({"person": person_id.to_string()})
        );
    }
}
