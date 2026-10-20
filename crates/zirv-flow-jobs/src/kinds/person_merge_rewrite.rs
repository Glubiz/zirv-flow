//! `person.merge.rewrite`: re-points the person-scoped rows that the merge transaction does
//! not move to the person a merged person was merged into.
//!
//! The payload is `{workspace_id, merged_person_id}`. The job reads the survivor from the
//! merged person's `merged_into`, so a later merge of the survivor is followed. It re-points
//! in batches of 1,000 rows per statement (after locking the merged person and the survivor, so an
//! erasure of the group waits for it), appends a `person.merge.rewrite` decision when it
//! is done, and a re-run re-points nothing more.

use serde_json::Value;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

use crate::{JobContext, JobError, JobKind};

pub(crate) struct PersonMergeRewrite;

impl JobKind for PersonMergeRewrite {
    const KIND: &'static str = zirv_flow_store::REWRITE_JOB_KIND;

    async fn run(&self, ctx: &mut JobContext<'_>, payload: Value) -> Result<(), JobError> {
        let (workspace, merged) = parse(&payload)?;
        if ctx.workspace_id() != Some(workspace) {
            return Err(JobError::new(
                "the payload names another workspace than the job",
            ));
        }
        let trace = ctx.trace().clone();
        let job = Some((ctx.job_id(), ctx.attempts()));
        zirv_flow_store::rewrite_merged_person(ctx.conn(), workspace, merged, &trace, job).await?;
        Ok(())
    }
}

fn parse(payload: &Value) -> Result<(WorkspaceId, Uuid), JobError> {
    let bad = JobError::new("the payload needs a workspace_id and a merged_person_id");
    let workspace = payload
        .get("workspace_id")
        .and_then(Value::as_str)
        .and_then(|text| text.parse::<WorkspaceId>().ok())
        .ok_or(bad)?;
    let merged = payload
        .get("merged_person_id")
        .and_then(Value::as_str)
        .and_then(|text| Uuid::parse_str(text).ok())
        .ok_or(bad)?;
    Ok((workspace, merged))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use sqlx::PgPool;
    use time::macros::datetime;
    use zirv_flow_core::{Cause, Slug, TraceContext, WorkspaceName};
    use zirv_flow_store::{ProfileOutcome, ProfileRecord, apply_profile_batch};

    use super::*;

    #[test]
    fn the_kind_name_is_person_merge_rewrite() {
        assert_eq!(PersonMergeRewrite::KIND, "person.merge.rewrite");
    }

    #[test]
    fn the_payload_needs_a_workspace_and_a_merged_person() {
        let ws = WorkspaceId::new();
        let person = Uuid::now_v7();

        let parsed = parse(&json!({"workspace_id": ws, "merged_person_id": person})).unwrap();

        assert_eq!(parsed, (ws, person));
        assert!(parse(&json!({"merged_person_id": person})).is_err());
        assert!(parse(&json!({"workspace_id": ws})).is_err());
        assert!(parse(&json!({"workspace_id": ws, "merged_person_id": 5})).is_err());
    }

    fn record(key: &str, identifiers: &[(&str, &str)]) -> ProfileRecord {
        ProfileRecord {
            source: "crm".into(),
            stream: "s".into(),
            record_key: key.into(),
            identifiers: identifiers
                .iter()
                .map(|(kind, value)| ((*kind).into(), (*value).into()))
                .collect(),
            traits: json!({}).as_object().cloned().unwrap(),
            observed_at: datetime!(2026-01-01 00:00 UTC),
            trace: TraceContext::root(Cause::api("t")),
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_job_re_points_conflicts_and_a_second_run_moves_nothing(pool: PgPool) {
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
                record("1", &[("external:crm", "1")]),
                record("2", &[("anonymous_id", "a")]),
                record("3", &[("email", "x@example.com")]),
            ],
        )
        .await
        .unwrap();
        let ids: Vec<Uuid> = outcomes
            .iter()
            .map(|outcome| match outcome {
                ProfileOutcome::Applied { person_id, .. } => *person_id,
                other => panic!("not applied: {other:?}"),
            })
            .collect();
        let (survivor, loser, third) = (ids[0], ids[1], ids[2]);
        let source_record: Uuid =
            sqlx::query_scalar("SELECT id FROM source_records WHERE record_key = '3'")
                .fetch_one(&pool)
                .await
                .unwrap();
        sqlx::query(
            "INSERT INTO identity_conflicts \
                (id, workspace_id, person_id, other_person_id, kind, source_record_id, reason) \
             VALUES ($1, $2, $3, $4, 'email', $5, 'unique_kind')",
        )
        .bind(Uuid::now_v7())
        .bind(ws.as_uuid())
        .bind(third)
        .bind(loser)
        .bind(source_record)
        .execute(&pool)
        .await
        .unwrap();
        apply_profile_batch(
            &pool,
            ws,
            &[record("4", &[("external:crm", "1"), ("anonymous_id", "a")])],
        )
        .await
        .unwrap();
        let claimed = zirv_flow_store::claim_job(
            &pool,
            "w",
            &[PersonMergeRewrite::KIND],
            Duration::from_secs(30),
            None,
        )
        .await
        .unwrap()
        .expect("the merge enqueued the rewrite");
        assert_eq!(
            claimed.payload,
            json!({"workspace_id": ws, "merged_person_id": loser})
        );

        for _ in 0..2 {
            let mut tx = zirv_flow_store::begin(&pool).await.unwrap();
            let mut ctx = JobContext::new(&mut tx, &claimed);
            PersonMergeRewrite
                .run(&mut ctx, claimed.payload.clone())
                .await
                .unwrap();
            zirv_flow_store::commit(tx).await.unwrap();
        }

        let other: Uuid = sqlx::query_scalar(
            "SELECT other_person_id FROM identity_conflicts WHERE person_id = $1",
        )
        .bind(third)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(other, survivor);
        let (records, _) = zirv_flow_store::list_trace(&pool, ws, claimed.trace.trace_id, 20)
            .await
            .unwrap();
        let rewrites: Vec<_> = records
            .iter()
            .filter(|record| record.action == "person.merge.rewrite")
            .collect();
        assert_eq!(rewrites.len(), 2);
        assert!(
            rewrites
                .iter()
                .all(|record| record.job_id == Some(claimed.id) && record.outcome == "completed")
        );
    }
}
