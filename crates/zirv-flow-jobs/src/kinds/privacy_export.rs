//! `privacy.export`: builds one person export (architecture section 16.3).
//!
//! The payload is `{workspace_id, export_id}`. The document is built and stored in the job's
//! own transaction; an export whose person was erased in the meantime is gone, and the job
//! finishes without doing anything.

use serde_json::Value;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

use crate::{JobContext, JobError, JobKind};

pub(crate) struct PrivacyExport;

impl JobKind for PrivacyExport {
    const KIND: &'static str = zirv_flow_store::EXPORT_JOB_KIND;

    async fn run(&self, ctx: &mut JobContext<'_>, payload: Value) -> Result<(), JobError> {
        let (workspace, export) = parse(&payload)?;
        if ctx.workspace_id() != Some(workspace) {
            return Err(JobError::new(
                "the payload names another workspace than the job",
            ));
        }
        zirv_flow_store::build_export(ctx.conn(), workspace, export).await?;
        Ok(())
    }
}

fn parse(payload: &Value) -> Result<(WorkspaceId, Uuid), JobError> {
    let bad = JobError::new("the payload needs a workspace_id and an export_id");
    let workspace = payload
        .get("workspace_id")
        .and_then(Value::as_str)
        .and_then(|text| text.parse::<WorkspaceId>().ok())
        .ok_or(bad)?;
    let export = payload
        .get("export_id")
        .and_then(Value::as_str)
        .and_then(|text| Uuid::parse_str(text).ok())
        .ok_or(bad)?;
    Ok((workspace, export))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use sqlx::PgPool;
    use time::macros::datetime;
    use zirv_flow_core::{Cause, Slug, TraceContext, WorkspaceName};
    use zirv_flow_store::{ExportDownload, ExportStatus, ProfileOutcome, ProfileRecord};

    use super::*;

    #[test]
    fn the_kind_name_is_privacy_export() {
        assert_eq!(PrivacyExport::KIND, "privacy.export");
    }

    #[test]
    fn the_payload_needs_a_workspace_and_an_export() {
        let ws = WorkspaceId::new();
        let export = Uuid::now_v7();

        let parsed = parse(&json!({"workspace_id": ws, "export_id": export})).unwrap();

        assert_eq!(parsed, (ws, export));
        assert!(parse(&json!({"export_id": export})).is_err());
        assert!(parse(&json!({"workspace_id": ws, "export_id": 1})).is_err());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_job_builds_the_export_and_a_second_run_changes_nothing(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let name = WorkspaceName::parse("ours").unwrap();
        let ws = zirv_flow_store::create_workspace(&pool, &name, &Slug::parse("ours").unwrap())
            .await
            .unwrap()
            .id;
        let trace = TraceContext::root(Cause::api("t"));
        let outcomes = zirv_flow_store::apply_profile_batch(
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
        let mut tx = zirv_flow_store::begin(&pool).await.unwrap();
        let export = zirv_flow_store::request_export(&mut tx, ws, person_id, None, &trace)
            .await
            .unwrap()
            .unwrap();
        zirv_flow_store::commit(tx).await.unwrap();
        let claimed = zirv_flow_store::claim_job(
            &pool,
            "w",
            &[PrivacyExport::KIND],
            Duration::from_secs(30),
            None,
        )
        .await
        .unwrap()
        .expect("the request enqueued the job");

        for _ in 0..2 {
            let mut tx = zirv_flow_store::begin(&pool).await.unwrap();
            let mut ctx = JobContext::new(&mut tx, &claimed);
            PrivacyExport
                .run(&mut ctx, claimed.payload.clone())
                .await
                .unwrap();
            zirv_flow_store::commit(tx).await.unwrap();
        }

        let stored = zirv_flow_store::find_privacy_export(&pool, ws, export.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.status, ExportStatus::Ready);
        assert!(matches!(
            zirv_flow_store::download_export(&pool, ws, export.id)
                .await
                .unwrap(),
            ExportDownload::Content(_)
        ));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_export_that_is_gone_finishes_without_error(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let name = WorkspaceName::parse("ours").unwrap();
        let ws = zirv_flow_store::create_workspace(&pool, &name, &Slug::parse("ours").unwrap())
            .await
            .unwrap()
            .id;
        let mut job =
            zirv_flow_store::NewJob::new(PrivacyExport::KIND, TraceContext::root(Cause::api("t")));
        job.workspace_id = Some(ws);
        job.payload = json!({"workspace_id": ws, "export_id": Uuid::now_v7()});
        zirv_flow_store::enqueue(&pool, &job).await.unwrap();
        let claimed = zirv_flow_store::claim_job(
            &pool,
            "w",
            &[PrivacyExport::KIND],
            Duration::from_secs(30),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        let mut tx = zirv_flow_store::begin(&pool).await.unwrap();
        let mut ctx = JobContext::new(&mut tx, &claimed);

        let result = PrivacyExport.run(&mut ctx, claimed.payload.clone()).await;

        assert!(result.is_ok());
    }
}
