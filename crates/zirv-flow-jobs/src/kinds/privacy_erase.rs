//! `privacy.erase`: carries one erasure request out (architecture section 16.3).
//!
//! The payload is `{workspace_id, request_id}`. The whole erasure runs in the job's own
//! transaction, so a failure leaves nothing done and the request pending for the retry; once
//! the attempts run out the request reads as failed. Nothing here touches an identifier.

use serde_json::Value;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_store::EraseOutcome;

use crate::{JobContext, JobError, JobKind};

pub(crate) struct PrivacyErase;

impl JobKind for PrivacyErase {
    const KIND: &'static str = zirv_flow_store::ERASE_JOB_KIND;

    async fn run(&self, ctx: &mut JobContext<'_>, payload: Value) -> Result<(), JobError> {
        let (workspace, request) = parse(&payload)?;
        if ctx.workspace_id() != Some(workspace) {
            return Err(JobError::new(
                "the payload names another workspace than the job",
            ));
        }
        let trace = ctx.trace().clone();
        match zirv_flow_store::erase_person(ctx.conn(), workspace, request, &trace).await? {
            EraseOutcome::Completed { .. } | EraseOutcome::AlreadyCompleted => Ok(()),
            EraseOutcome::NoSuchRequest => Err(JobError::new("the erasure request does not exist")),
        }
    }
}

fn parse(payload: &Value) -> Result<(WorkspaceId, Uuid), JobError> {
    let bad = JobError::new("the payload needs a workspace_id and a request_id");
    let workspace = payload
        .get("workspace_id")
        .and_then(Value::as_str)
        .and_then(|text| text.parse::<WorkspaceId>().ok())
        .ok_or(bad)?;
    let request = payload
        .get("request_id")
        .and_then(Value::as_str)
        .and_then(|text| Uuid::parse_str(text).ok())
        .ok_or(bad)?;
    Ok((workspace, request))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use sqlx::PgPool;
    use time::macros::datetime;
    use zirv_flow_core::{Cause, HmacPurpose, MasterKey, Slug, TraceContext, WorkspaceName};
    use zirv_flow_store::{ErasureStatus, ProfileOutcome, ProfileRecord, apply_profile_batch};

    use super::*;

    #[test]
    fn the_kind_name_is_privacy_erase() {
        assert_eq!(PrivacyErase::KIND, "privacy.erase");
    }

    #[test]
    fn the_payload_needs_a_workspace_and_a_request() {
        let ws = WorkspaceId::new();
        let request = Uuid::now_v7();

        let parsed = parse(&json!({"workspace_id": ws, "request_id": request})).unwrap();

        assert_eq!(parsed, (ws, request));
        assert!(parse(&json!({"request_id": request})).is_err());
        assert!(parse(&json!({"workspace_id": ws})).is_err());
        assert!(parse(&json!({"workspace_id": ws, "request_id": 5})).is_err());
    }

    async fn run_claimed(pool: &PgPool) -> Result<(), JobError> {
        let claimed = zirv_flow_store::claim_job(
            pool,
            "w",
            &[PrivacyErase::KIND],
            Duration::from_secs(30),
            None,
        )
        .await
        .unwrap()
        .expect("the request enqueued the job");
        let mut tx = zirv_flow_store::begin(pool).await.unwrap();
        let mut ctx = JobContext::new(&mut tx, &claimed);
        let result = PrivacyErase.run(&mut ctx, claimed.payload.clone()).await;
        if result.is_ok() {
            zirv_flow_store::commit(tx).await.unwrap();
        }
        result
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_job_erases_the_person_suppresses_the_email_and_completes_the_request(
        pool: PgPool,
    ) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let name = WorkspaceName::parse("ours").unwrap();
        let ws = zirv_flow_store::create_workspace(&pool, &name, &Slug::parse("ours").unwrap())
            .await
            .unwrap()
            .id;
        let trace = TraceContext::root(Cause::api("t"));
        let outcomes = apply_profile_batch(
            &pool,
            ws,
            &[ProfileRecord {
                source: "crm".into(),
                stream: "s".into(),
                record_key: "1".into(),
                identifiers: vec![("email".into(), "ada@example.com".into())],
                traits: json!({"plan": "pro"}).as_object().cloned().unwrap(),
                observed_at: datetime!(2026-01-01 00:00 UTC),
                trace: trace.clone(),
            }],
        )
        .await
        .unwrap();
        let ProfileOutcome::Applied { person_id, .. } = outcomes[0] else {
            panic!("not applied");
        };
        let key = MasterKey::generate().hmac_key(HmacPurpose::Identifier);
        let mut tx = zirv_flow_store::begin(&pool).await.unwrap();
        let request = zirv_flow_store::request_erasure(&mut tx, ws, person_id, &key, None, &trace)
            .await
            .unwrap()
            .unwrap();
        zirv_flow_store::commit(tx).await.unwrap();

        run_claimed(&pool).await.unwrap();

        assert!(
            zirv_flow_store::find_person(&pool, ws, person_id)
                .await
                .unwrap()
                .is_none()
        );
        let hash = zirv_flow_core::people::identifier_hash(
            &key,
            &zirv_flow_core::people::Identifier::parse("email", "ada@example.com").unwrap(),
        );
        assert!(
            zirv_flow_store::is_suppressed(&pool, ws, &hash)
                .await
                .unwrap()
        );
        let stored = zirv_flow_store::find_erasure_request(&pool, ws, request.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.status, ErasureStatus::Completed);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_request_that_does_not_exist_fails_the_job_with_a_fixed_message(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let name = WorkspaceName::parse("ours").unwrap();
        let ws = zirv_flow_store::create_workspace(&pool, &name, &Slug::parse("ours").unwrap())
            .await
            .unwrap()
            .id;
        let mut job =
            zirv_flow_store::NewJob::new(PrivacyErase::KIND, TraceContext::root(Cause::api("t")));
        job.workspace_id = Some(ws);
        job.payload = json!({"workspace_id": ws, "request_id": Uuid::now_v7()});
        zirv_flow_store::enqueue(&pool, &job).await.unwrap();

        let error = run_claimed(&pool).await.unwrap_err();

        assert_eq!(error.message(), "the erasure request does not exist");
    }
}
